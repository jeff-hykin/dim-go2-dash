// The on-screen touch sticks' math (Sticks.tsx draws them): a finger's offset from the stick's centre, as a gamepad
// stick would report it, so the drive's axes come from gamepad.ts's padAxes (same dead zone and curve).

/** The finger's offset from the centre, -1..1 per axis, clamped to the unit circle; y down = + (a gamepad's). */
export function stickOffset(dx: number, dy: number, radius: number): { x: number; y: number } {
    if (!(radius > 0)) {
        return { x: 0, y: 0 }
    }
    const x = dx / radius
    const y = dy / radius
    const length = Math.hypot(x, y)
    return length > 1 ? { x: x / length, y: y / length } : { x, y }
}
