import { render } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { TextBlockLayer } from '@/components/canvas/TextBlockLayer'

const hooks = vi.hoisted(() => ({
  useBlobImage: vi.fn(),
  useCurrentPage: vi.fn(),
  useTextNodes: vi.fn(),
}))

vi.mock('@/hooks/useBlobData', () => ({ useBlobImage: hooks.useBlobImage }))
vi.mock('@/hooks/useCurrentPage', () => ({
  useCurrentPage: hooks.useCurrentPage,
  useTextNodes: hooks.useTextNodes,
}))

describe('TextBlockLayer sprite placement', () => {
  beforeEach(() => {
    hooks.useBlobImage.mockReturnValue({ data: 'blob:sprite' })
    hooks.useCurrentPage.mockReturnValue({ id: 'page-1' })
    hooks.useTextNodes.mockReturnValue([
      {
        id: 'text-1',
        transform: { x: 10, y: 20, width: 30, height: 10, rotationDeg: 90 },
        data: {
          raw: 'source',
          sprite: 'sprite-ref',
          spriteTransform: { x: 10, y: 20, width: 30, height: 10, rotationDeg: 90 },
        },
      },
    ])
  })

  it('scales the rotated sprite frame from its top-left so its center stays aligned', () => {
    const { container } = render(<TextBlockLayer showSprites scale={2} />)
    const sprite = container.querySelector('img')
    const frame = sprite?.parentElement

    expect(frame).toHaveStyle({ left: '20px', top: '40px', transform: 'scale(2)' })
    expect(frame).toHaveStyle({ transformOrigin: 'top left' })
    expect(sprite).toHaveStyle({ transform: 'rotate(90deg)', transformOrigin: 'center' })
  })
})
