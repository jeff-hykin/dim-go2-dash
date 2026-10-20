// deno test frontend/test — the touch sticks' finger → axes (sticks.ts, then gamepad.ts's padAxes, as Sticks.tsx does).
import { padAxes } from "../src/gamepad.ts"
import { stickOffset } from "../src/sticks.ts"

function assertEquals(actual: unknown, expected: unknown) {
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
        throw new Error(`expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`)
    }
}

const axes = (move: [number, number], turnX: number, radius = 60) => {
    const m = stickOffset(move[0], move[1], radius)
    const t = stickOffset(turnX, 0, radius)
    return padAxes({ lx: m.x, ly: m.y, rx: t.x })
}

Deno.test("a finger past the ring is clamped to full speed", () => {
    assertEquals(stickOffset(0, -200, 60), { x: 0, y: -1 })
    assertEquals(stickOffset(300, 400, 50), { x: 0.6, y: 0.8 })
})

Deno.test("up is forward, left strafes left, the right stick to the left turns left", () => {
    assertEquals(axes([0, -60], 0), { forward: 1, strafe: 0, turn: 0 })
    assertEquals(axes([-60, 0], 0), { forward: 0, strafe: 1, turn: 0 })
    assertEquals(axes([0, 0], -60), { forward: 0, strafe: 0, turn: 1 })
    assertEquals(axes([0, 60], 60), { forward: -1, strafe: 0, turn: -1 })
})

Deno.test("inside the dead zone (and at rest) nothing moves", () => {
    assertEquals(axes([5, -5], 6), { forward: 0, strafe: 0, turn: 0 })
    assertEquals(axes([0, 0], 0), { forward: 0, strafe: 0, turn: 0 })
    assertEquals(stickOffset(10, 10, 0), { x: 0, y: 0 })
})
