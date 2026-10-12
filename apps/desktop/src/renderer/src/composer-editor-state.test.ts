import { describe, expect, it } from 'vitest'
import type { ComposerDocument } from '@contracts'
import {
  $createParagraphNode,
  $createNodeSelection,
  $createTextNode,
  $getRoot,
  $getSelection,
  $isDecoratorNode,
  $isElementNode,
  $isLineBreakNode,
  $isNodeSelection,
  $isParagraphNode,
  $isRangeSelection,
  $isTextNode,
  $setSelection,
  createEditor,
  type LexicalEditor
} from 'lexical'
import { ComposerAtomNode, $isComposerAtomNode } from './ComposerAtomNode'
import {
  $insertComposerAtomWithTrailingSpace,
  $replaceEditorWithComposerDocument,
  $selectedComposerDocument,
  composerDocumentToEditorState,
  editorStateToComposerDocument
} from './composer-editor-state'
import {
  $deleteComposerAtomSelection,
  $moveComposerAtomSelection
} from './RovaiComposerExtension'

function createComposerEditor(): LexicalEditor {
  return createEditor({
    namespace: `ComposerTest-${crypto.randomUUID()}`,
    nodes: [ComposerAtomNode],
    onError(error) { throw error }
  })
}

describe('Composer Lexical state boundary', () => {
  it('imports Text + Atom into one Paragraph with explicit LineBreak nodes', () => {
    const editor = createComposerEditor()
    const state = composerDocumentToEditorState(editor, {
      version: 2,
      segments: [
        { kind: 'text', text: '请让 ' },
        { kind: 'atom', atom: { type: 'member', agentId: 'agent-a' } },
        { kind: 'text', text: ' 处理\n下一项' }
      ]
    })

    state.read(() => {
      const children = $getRoot().getChildren()
      expect(children).toHaveLength(1)
      expect($isParagraphNode(children[0])).toBe(true)
      if (!$isElementNode(children[0])) throw new Error('expected paragraph')
      const paragraphChildren = children[0].getChildren()
      expect(paragraphChildren.map((node) => node.getType())).toEqual([
        'text', 'composer-atom', 'text', 'linebreak', 'text'
      ])
      expect($isLineBreakNode(paragraphChildren[3])).toBe(true)
      expect($isComposerAtomNode(paragraphChildren[1])).toBe(true)
    })
  })

  it('round-trips identity while excluding selection, keys and presentation state', () => {
    const editor = createComposerEditor()
    const document = {
      version: 2 as const,
      segments: [
        { kind: 'text' as const, text: 'A' },
        {
          kind: 'atom' as const,
          atom: { type: 'skill' as const, skillId: 'skill-review', nameAtSend: 'review-pr' }
        },
        { kind: 'text' as const, text: '\nB' }
      ]
    }
    const state = composerDocumentToEditorState(editor, document)

    expect(editorStateToComposerDocument(state)).toEqual(document)
    expect(JSON.stringify(editorStateToComposerDocument(state))).not.toMatch(
      /selection|presentation|__key|nodeKey/
    )
  })

  it('converts multiple external Paragraph boundaries to domain newlines', () => {
    const editor = createComposerEditor()
    editor.update(() => {
      const root = $getRoot()
      root.clear()
      root.append(
        $createParagraphNode().append($createTextNode('第一段')),
        $createParagraphNode().append($createTextNode('第二段'))
      )
    }, { discrete: true })

    expect(editorStateToComposerDocument(editor.getEditorState())).toEqual({
      version: 2,
      segments: [{ kind: 'text', text: '第一段\n第二段' }]
    })
  })

  it('serializes one identity-only inline Decorator Atom', () => {
    const editor = createComposerEditor()
    composerDocumentToEditorState(editor, {
      version: 2,
      segments: [{
        kind: 'atom',
        atom: { type: 'member', agentId: 'agent-a', labelFallback: '洛可' }
      }]
    })

    editor.getEditorState().read(() => {
      const atom = $getRoot().getFirstDescendant()
      expect($isComposerAtomNode(atom)).toBe(true)
      if (!$isComposerAtomNode(atom)) return
      expect($isDecoratorNode(atom)).toBe(true)
      expect(atom.isInline()).toBe(true)
      expect(atom.isKeyboardSelectable()).toBe(true)
      expect(atom.isIsolated()).toBe(false)
      expect(atom.getTextContent()).toBe('')
    })
    const atomJson = (editor.getEditorState().toJSON().root.children[0] as unknown as {
      children: Array<Record<string, unknown>>
    }).children[0]
    expect(atomJson).toMatchObject({
      type: 'composer-atom',
      referenceId: 'agent-a',
      fallbackLabel: '洛可'
    })
    expect(atomJson).not.toHaveProperty('text')
    expect(atomJson).not.toHaveProperty('mode')
    expect(atomJson).not.toHaveProperty('presentationState')
  })

  it('reuses right-side whitespace when replacing a Typeahead query with an Atom', () => {
    const editor = createComposerEditor()
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [{ kind: 'text', text: '/rev 继续' }]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const text = paragraph.getFirstChildOrThrow()
      if (text.getType() !== 'text') throw new Error('expected query text')
      const [query] = (text as ReturnType<typeof $createTextNode>).splitText(4)
      $insertComposerAtomWithTrailingSpace(query, {
        type: 'skill', skillId: 'skill-review', nameAtSend: 'review-pr'
      })
    }, { discrete: true })

    expect(editorStateToComposerDocument(editor.getEditorState())).toEqual({
      version: 2,
      segments: [
        {
          kind: 'atom',
          atom: { type: 'skill', skillId: 'skill-review', nameAtSend: 'review-pr' }
        },
        { kind: 'text', text: ' 继续' }
      ]
    })
  })

  it('projects a keyboard-selected Decorator Atom for structured clipboard copy', () => {
    const editor = createComposerEditor()
    let selected: ComposerDocument = { version: 2, segments: [] }
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [{
          kind: 'atom',
          atom: { type: 'skill', skillId: 'skill-review', nameAtSend: 'review-pr' }
        }]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const atom = paragraph.getFirstChild()
      if (!$isComposerAtomNode(atom)) throw new Error('expected atom')
      const selection = $createNodeSelection()
      selection.add(atom.getKey())
      $setSelection(selection)
      selected = $selectedComposerDocument()
    }, { discrete: true })

    expect(selected).toEqual({
      version: 2,
      segments: [{
        kind: 'atom',
        atom: { type: 'skill', skillId: 'skill-review', nameAtSend: 'review-pr' }
      }]
    })
  })

  it('moves a caret off a keyboard-selected Atom so the cursor stays visible', () => {
    const editor = createComposerEditor()
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-a', labelFallback: '甲' } },
          { kind: 'text', text: ' ' },
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-b', labelFallback: '乙' } },
          { kind: 'text', text: ' ' }
        ]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const secondAtom = paragraph.getChildren()[2]
      if (!$isComposerAtomNode(secondAtom)) throw new Error('expected second atom')
      const selection = $createNodeSelection()
      selection.add(secondAtom.getKey())
      $setSelection(selection)

      // The arrow escape mirrors the caret step that produced this NodeSelection.
      expect($moveComposerAtomSelection('previous')).toBe(true)
      const after = $getSelection()
      expect($isRangeSelection(after)).toBe(true)
      if (!$isRangeSelection(after)) return
      expect(after.isCollapsed()).toBe(true)
      const node = after.anchor.getNode()
      expect(node.getType()).toBe('text')
      expect(node.getTextContent()).toBe(' ')
      expect(after.anchor.offset).toBe(1)
    }, { discrete: true })
  })

  it('moves a caret past the last keyboard-selected Atom on ArrowRight', () => {
    const editor = createComposerEditor()
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-a', labelFallback: '甲' } },
          { kind: 'text', text: ' ' },
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-b', labelFallback: '乙' } },
          { kind: 'text', text: '尾' }
        ]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const [firstAtom, , secondAtom] = paragraph.getChildren()
      const selection = $createNodeSelection()
      selection.add(firstAtom.getKey())
      selection.add(secondAtom.getKey())
      $setSelection(selection)

      expect($moveComposerAtomSelection('next')).toBe(true)
      const after = $getSelection()
      expect($isRangeSelection(after)).toBe(true)
      if (!$isRangeSelection(after)) return
      expect(after.isCollapsed()).toBe(true)
      expect(after.anchor.getNode().getTextContent()).toBe('尾')
      expect(after.anchor.offset).toBe(0)
    }, { discrete: true })
  })

  it('deletes a keyboard-selected Atom on Backspace and restores a visible caret', () => {
    const editor = createComposerEditor()
    let afterDocument: ComposerDocument = { version: 2, segments: [] }
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-a', labelFallback: '甲' } },
          { kind: 'text', text: ' ' },
          { kind: 'atom', atom: { type: 'member', agentId: 'agent-b', labelFallback: '乙' } },
          { kind: 'text', text: ' ' }
        ]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const secondAtom = paragraph.getChildren()[2]
      if (!$isComposerAtomNode(secondAtom)) throw new Error('expected second atom')
      const selection = $createNodeSelection()
      selection.add(secondAtom.getKey())
      $setSelection(selection)

      expect($deleteComposerAtomSelection()).toBe(true)
      const after = $getSelection()
      expect($isRangeSelection(after)).toBe(true)
      if (!$isRangeSelection(after)) return
      expect(after.isCollapsed()).toBe(true)
    }, { discrete: true })

    afterDocument = editorStateToComposerDocument(editor.getEditorState())
    expect(afterDocument.segments.map((segment) => segment.kind)).toEqual(['atom', 'text'])
    expect(afterDocument.segments[0]).toEqual({
      kind: 'atom',
      atom: { type: 'member', agentId: 'agent-a', labelFallback: '甲' }
    })
  })

  it('leaves non-atom selections untouched by the atom selection helpers', () => {
    const editor = createComposerEditor()
    editor.update(() => {
      $replaceEditorWithComposerDocument({
        version: 2,
        segments: [{ kind: 'text', text: 'hello' }]
      })
      const paragraph = $getRoot().getFirstChildOrThrow()
      if (!$isElementNode(paragraph)) throw new Error('expected paragraph')
      const text = paragraph.getFirstChild()
      if (!$isTextNode(text)) throw new Error('expected text')
      text.select(1, 3)
      expect($moveComposerAtomSelection('previous')).toBe(false)
      expect($deleteComposerAtomSelection()).toBe(false)
    }, { discrete: true })
  })
})
