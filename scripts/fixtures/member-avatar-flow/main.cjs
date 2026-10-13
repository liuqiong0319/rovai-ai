const assert = require('node:assert/strict')
const { mkdir, writeFile } = require('node:fs/promises')
const { join, resolve } = require('node:path')
const { pathToFileURL } = require('node:url')
const { createInterface } = require('node:readline')
const { app, BrowserWindow, ipcMain, nativeImage } = require('electron')
const [renderer, userData, coreBinary, sourceFile] = process.argv.slice(2)
const root = resolve(__dirname, '../../..')
const dataDir = join(userData, 'core')
app.setPath('userData', userData)
app.setPath('sessionData', join(userData, 'session'))
let core, window
const calls = []
const delay = ms => new Promise(resolve => setTimeout(resolve, ms))

app.whenReady().then(async () => {
  const { launchHost } = await import(pathToFileURL(join(root, 'scripts/lib/host-test-client.mjs')).href)
  const { coreDataDirectoryArguments } = await import(pathToFileURL(join(root, 'scripts/lib/runtime-camp-files-root.mjs')).href)
  await mkdir(dataDir, { recursive: true })
  console.log(`Automatic acceptance Core: ${coreBinary}; userData: ${userData}; Skill Library: ${join(dataDir, 'skills')}; MCP: ${join(dataDir, 'mcp.json')}`)
  core = launchHost(coreBinary, [...coreDataDirectoryArguments(dataDir), '--skill-library-root', join(dataDir, 'skills'),
    '--mcp-config-path', join(dataDir, 'mcp.json')])
  createInterface({ input: core.child.stdout }).on('line', line => {
    const frame = JSON.parse(line)
    if (frame.kind === 'core_startup' && frame.error) console.error(frame.error)
  })
  await core.ready
  ipcMain.handle('avatar-fixture:request', async (_, method, params) => {
    assert.ok(['memberAvatars.read', 'memberAvatars.save'].includes(method))
    const result = await core.request(method, params)
    calls.push({ method, kind: params.kind, crop: params.crop, avatarRef: params.avatarRef })
    return result
  })
  window = new BrowserWindow({ width: 1000, height: 1000, show: true,
    webPreferences: { preload: join(__dirname, 'preload.cjs'), sandbox: true, contextIsolation: true, backgroundThrottling: false } })
  window.webContents.on('console-message', (_, details) => {
    if (details.level === 'error') console.error(details.message)
  })
  const run = expression => window.webContents.executeJavaScript(expression)
  const waitFor = async expression => {
    const deadline = Date.now() + 15_000
    while (Date.now() < deadline) { if (await run(expression)) return; await delay(50) }
    throw new Error(`Timed out: ${expression}; ${await run('document.body.innerText')}`)
  }
  const click = selector => run(`document.querySelector(${JSON.stringify(selector)}).click()`)
  const cropReady = "Boolean(document.querySelector('.avatar-crop-stage img')?.naturalWidth) && !document.querySelector('.member-editor-crop-actions .member-editor-secondary').disabled"
  await window.loadFile(renderer)
  window.webContents.debugger.attach('1.3')
  await window.webContents.debugger.sendCommand('Page.setInterceptFileChooserDialog', { enabled: true })
  await waitFor("Boolean(document.querySelector('.member-editor-avatar-upload button'))")
  await click('.member-editor-avatar-upload button')
  const { root: documentNode } = await window.webContents.debugger.sendCommand('DOM.getDocument')
  const { nodeId } = await window.webContents.debugger.sendCommand('DOM.querySelector', { nodeId: documentNode.nodeId, selector: 'input[type="file"]' })
  await window.webContents.debugger.sendCommand('DOM.setFileInputFiles', { nodeId, files: [sourceFile] })
  await waitFor(cropReady)
  const initialUrl = await run("document.querySelector('.avatar-crop-stage img').src")
  const initialTransform = await run("document.querySelector('.avatar-crop-stage img').style.transform")
  await run("document.querySelector('.avatar-crop-stage').focus()")
  window.webContents.sendInputEvent({ type: 'keyDown', keyCode: 'Right' })
  window.webContents.sendInputEvent({ type: 'keyUp', keyCode: 'Right' })
  await waitFor(`document.querySelector('.avatar-crop-stage img').style.transform !== ${JSON.stringify(initialTransform)}`)
  assert.equal(await run("document.querySelector('.avatar-crop-stage img').src"), initialUrl,
    'Moving the crop retains the animation URL')
  const center = await run("(() => { const r = document.querySelector('.avatar-crop-stage').getBoundingClientRect(); return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2), width: 1, height: 1 } })()")
  const displayed = new Set()
  for (let i = 0; i < 8; i++) {
    const pixel = (await window.webContents.capturePage(center)).getBitmap()
    displayed.add(pixel.subarray(0, 3).toString('hex'))
    await delay(100)
  }
  assert.ok(displayed.size >= 2, 'The actual crop preview renders multiple GIF frames')
  await click('.member-editor-crop-actions .member-editor-secondary')
  await waitFor("Boolean(document.querySelector('[data-avatar-ref]').textContent) && document.querySelector('.member-editor-avatar-editor').hidden")
  const firstRef = await run("document.querySelector('[data-avatar-ref]').textContent")
  const portrait = await core.request('memberAvatars.read', { avatarRef: firstRef, rendition: 'portrait' })
  assert.equal(portrait.mediaType, 'image/gif')
  assert.deepEqual([portrait.width, portrait.height], [2048, 128])
  assert.equal(Buffer.from(portrait.base64, 'base64').subarray(0, 6).toString(), 'GIF89a')
  await click('[data-reopen]')
  await waitFor(cropReady)
  assert.equal(await run("document.querySelector('.avatar-crop-stage img').naturalHeight"), 128)
  await run("document.querySelector('.avatar-crop-stage').focus()")
  window.webContents.sendInputEvent({ type: 'keyDown', keyCode: 'Left' })
  window.webContents.sendInputEvent({ type: 'keyUp', keyCode: 'Left' })
  await delay(100)
  await click('.member-editor-crop-actions .member-editor-secondary')
  await waitFor("document.querySelector('.member-editor-avatar-editor').hidden")
  assert.deepEqual(calls.filter(call => call.method === 'memberAvatars.save').map(call => call.kind), ['gif', 'recrop'])
  const secondRef = await run("document.querySelector('[data-avatar-ref]').textContent")
  assert.notEqual(secondRef, firstRef, 'A changed crop produces its own content-addressed avatar')
  await window.loadFile(renderer)
  await waitFor("Boolean(document.querySelector('[data-reopen]'))")
  await click('[data-reopen]')
  await waitFor(cropReady)
  assert.equal(await run("document.querySelector('[data-avatar-ref]').textContent"), secondRef)
  const icon = await core.request('memberAvatars.read', { avatarRef: secondRef, rendition: 'icon' })
  assert.equal(icon.mediaType, 'image/gif')
  const cover = await core.request('memberAvatars.read', { avatarRef: secondRef, rendition: 'icon', staticPng: true })
  assert.equal(cover.mediaType, 'image/png')
  assert.deepEqual([cover.width, cover.height], [192, 192])
  const firstPixel = nativeImage.createFromBuffer(Buffer.from(cover.base64, 'base64')).getBitmap().subarray(0, 4)
  assert.deepEqual([...firstPixel], [0, 0, 255, 255], 'Channel cover uses the first complete red frame')
  console.log(JSON.stringify({ ok: true, cases: ['animated upload and crop preview', 'stable Blob URL during adjustment',
    'wide source normalized to 2048x128', 'save/reopen/recrop through Core', 'renderer reload', 'animated icon and static PNG channel cover'] }))
}).catch(async error => {
  process.exitCode = 1
  console.error(error)
  try {
    if (window && !window.isDestroyed()) await writeFile(join(userData, 'failure.png'), (await window.webContents.capturePage()).toPNG())
  } catch (captureError) { console.error('Failure screenshot unavailable:', captureError) }
}).finally(async () => {
  if (core) await core.close()
  if (window && !window.isDestroyed()) window.destroy()
  app.exit(process.exitCode ?? 0)
})
