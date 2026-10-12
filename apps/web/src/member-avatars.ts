import { MEMBER_AVATAR_LIMITS, type MemberAvatarRendition, type MemberAvatarsApi } from '@contracts'
import { validateDecodedMemberAvatarDimensions } from '../../desktop/src/renderer/src/member-avatar-image'
import type { ConsoleClient } from './client'

function base64(bytes: Uint8Array): string {
  let binary = ''
  for (let offset = 0; offset < bytes.length; offset += 8192) binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192))
  return btoa(binary)
}

function isGif(bytes: Uint8Array): boolean {
  return (
    bytes.length >= 6
    && (bytes[0] === 0x47 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x38)
    && ((bytes[4] === 0x37 && bytes[5] === 0x61) || (bytes[4] === 0x39 && bytes[5] === 0x61))
  )
}

/** Minimal GIF structure walk mirroring the Main inspector: logical screen
 * dimensions, frame count and the Netscape loop count, with the same frame and
 * cumulative decode resource caps. */
function inspectGif(bytes: Uint8Array): { width: number; height: number; frameCount: number; loopCount: number } {
  if (!isGif(bytes) || bytes.length < 13) throw new Error('GIF 角色图片结构无效')
  const width = bytes[6] | (bytes[7] << 8)
  const height = bytes[8] | (bytes[9] << 8)
  if (width <= 0 || height <= 0) throw new Error('GIF 角色图片尺寸无效')
  validateDecodedMemberAvatarDimensions(width, height)
  const packed = bytes[10]
  let offset = 13 + (packed & 0x80 ? 3 << ((packed & 0x07) + 1) : 0)
  if (offset > bytes.length) throw new Error('GIF 角色图片结构无效')
  let frameCount = 0
  let loopCount = 0
  const skipSubBlocks = (cursor: number): number => {
    for (;;) {
      const size = bytes[cursor]
      cursor += 1
      if (size === 0) return cursor
      cursor += size
      if (cursor > bytes.length) throw new Error('GIF 角色图片结构无效')
    }
  }
  while (offset < bytes.length) {
    const block = bytes[offset]
    if (block === 0x3b) break
    if (block === 0x21) {
      offset += 2
      if (bytes[offset - 1] === 0xff) {
        const size = bytes[offset]
        offset += 1
        if (bytesToAscii(bytes.subarray(offset, offset + size)) === 'NETSCAPE2.0') {
          offset += size
          const sub = bytes[offset]
          offset += 1
          if (sub >= 3 && bytes[offset] === 0x01) {
            loopCount = bytes[offset + 1] | (bytes[offset + 2] << 8)
          }
        }
      }
      offset = skipSubBlocks(offset)
      continue
    }
    if (block === 0x2c) {
      frameCount += 1
      if (frameCount > MEMBER_AVATAR_LIMITS.maximumGifFrames) {
        throw new Error('GIF 角色图片不能超过 {0} 帧'.replace('{0}', String(MEMBER_AVATAR_LIMITS.maximumGifFrames)))
      }
      offset += 9
      const imagePacked = bytes[offset]
      offset += 1 + (imagePacked & 0x80 ? 3 << ((imagePacked & 0x07) + 1) : 0) + 1
      offset = skipSubBlocks(offset)
      continue
    }
    throw new Error('GIF 角色图片结构无效')
  }
  if (frameCount === 0) throw new Error('GIF 角色图片没有帧')
  if (width * height * frameCount > MEMBER_AVATAR_LIMITS.maximumGifCumulativeDecodedPixels) {
    throw new Error('GIF 角色图片累计解码资源超出限制')
  }
  return { width, height, frameCount, loopCount }
}

function bytesToAscii(bytes: Uint8Array): string {
  return String.fromCharCode(...bytes)
}

export function browserMemberAvatars(transport: ConsoleClient): MemberAvatarsApi {
  return {
    selectSource: async () => {
      const file = await new Promise<File | null>(resolve => {
        const input = document.createElement('input')
        input.type = 'file'; input.accept = 'image/png,image/jpeg,image/gif'
        input.addEventListener('change', () => { resolve(input.files?.[0] ?? null); input.remove() }, { once: true })
        input.addEventListener('cancel', () => { resolve(null); input.remove() }, { once: true })
        input.hidden = true; document.body.append(input); input.click()
      })
      if (!file) return null
      if (file.size > MEMBER_AVATAR_LIMITS.selectedFileBytes) throw new Error('角色图片不能超过 10 MiB')
      if (file.type !== 'image/png' && file.type !== 'image/jpeg' && file.type !== 'image/gif') throw new Error('请选择 PNG、JPEG 或 GIF 图片')
      const bytes = new Uint8Array(await file.arrayBuffer())
      if (file.type === 'image/gif') {
        const gif = inspectGif(bytes)
        return { displayName: file.name, mediaType: 'image/gif', bytes, inspectedWidth: gif.width, inspectedHeight: gif.height, byteLength: file.size, frameCount: gif.frameCount, loopCount: gif.loopCount }
      }
      const bitmap = await createImageBitmap(file, { imageOrientation: 'from-image' })
      try {
        validateDecodedMemberAvatarDimensions(bitmap.width, bitmap.height)
        return { displayName: file.name, mediaType: file.type, bytes, inspectedWidth: bitmap.width, inspectedHeight: bitmap.height, byteLength: file.size }
      } finally { bitmap.close() }
    },
    save: input => {
      if (input.kind === 'gif') {
        return transport.avatar('save', {
          kind: 'gif', sourceGifBase64: base64(input.sourceGif),
          sourceWidth: input.sourceWidth, sourceHeight: input.sourceHeight, crop: input.crop
        })
      }
      return transport.avatar('save', {
        sourceBase64: base64(input.sourcePng), iconBase64: base64(input.iconPng),
        sourceWidth: input.sourceWidth, sourceHeight: input.sourceHeight, crop: input.crop
      })
    },
    read: async (avatarRef, rendition) => {
      const image = await transport.avatar<Omit<MemberAvatarRendition, 'bytes'> & { base64: string } | null>('read', { avatarRef, rendition })
      return image ? { ...image, bytes: Uint8Array.from(atob(image.base64), char => char.charCodeAt(0)) } : null
    }
  }
}
