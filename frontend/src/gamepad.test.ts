// deno test frontend/src/gamepad.test.ts — the pad's safety rules (gamepad.ts), with a fake pad and target.
import { type Axes, GamepadDriver, joySample, type PadLike, SIT_HOLD_MS } from "./gamepad.ts"

function assertEquals(actual: unknown, expected: unknown) {
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
        throw new Error(
            `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
        )
    }
}

function fake() {
    const pad: PadLike = {
        index: 0,
        id: "Steam Deck",
        mapping: "standard",
        connected: true,
        axes: [0, 0, 0, 0],
        buttons: Array.from({ length: 17 }, () => ({ pressed: false, value: 0 })),
    }
    const log: string[] = []
    let axes: Axes = { forward: 0, strafe: 0, turn: 0 }
    const driver = new GamepadDriver({
        setAxes: (a) => (axes = a),
        setBoost: (b) => log.push(`boost ${b}`),
        stop: () => log.push("stop"),
        sitDown: () => log.push("sit"),
    })
    const press = (i: number, down: boolean) => {
        ;(pad.buttons as { pressed: boolean; value: number }[])[i] = {
            pressed: down,
            value: down ? 1 : 0,
        }
    }
    return { pad, log, driver, press, axes: () => axes }
}

Deno.test("a stick held at connect drives nothing until it rests", () => {
    const { pad, driver, axes } = fake()
    pad.axes = [0, -1, 0, 0]
    driver.poll([pad], 0)
    assertEquals(axes(), { forward: 0, strafe: 0, turn: 0 })
    pad.axes = [0, 0, 0, 0]
    driver.poll([pad], 30)
    pad.axes = [0, -1, 0, 0]
    driver.poll([pad], 60)
    assertEquals(axes().forward, 1)
})

Deno.test("LT + RT stops and latches until A, then needs rest again", () => {
    const { pad, driver, press, log, axes } = fake()
    driver.poll([pad], 0)
    pad.axes = [0, -1, 0, 0]
    press(6, true)
    press(7, true)
    driver.poll([pad], 30)
    assertEquals(log, ["stop"])
    assertEquals(axes().forward, 0)
    press(6, false)
    press(7, false)
    press(0, true)
    driver.poll([pad], 60)
    press(0, false)
    driver.poll([pad], 90)
    assertEquals(axes().forward, 0) // stick still held: not at rest
    pad.axes = [0, 0, 0, 0]
    driver.poll([pad], 120)
    pad.axes = [0, -1, 0, 0]
    driver.poll([pad], 150)
    assertEquals(axes().forward, 1)
})

Deno.test("hold B: stop at once, sit down after a second, once", () => {
    const { pad, driver, press, log } = fake()
    driver.poll([pad], 0)
    press(1, true)
    driver.poll([pad], 10)
    assertEquals(log, ["stop"])
    driver.poll([pad], 10 + SIT_HOLD_MS / 2)
    assertEquals(log, ["stop"])
    driver.poll([pad], 20 + SIT_HOLD_MS)
    driver.poll([pad], 40 + SIT_HOLD_MS)
    assertEquals(log, ["stop", "sit"])
})

Deno.test("disconnect zeroes", () => {
    const { pad, driver, axes } = fake()
    driver.poll([pad], 0)
    pad.axes = [0.5, 0, 0, 0]
    driver.poll([pad], 30)
    assertEquals(axes().strafe < 0, true)
    driver.poll([null], 60)
    assertEquals(axes(), { forward: 0, strafe: 0, turn: 0 })
})

Deno.test("Joy samples are the raw axes and 0/1 buttons", () => {
    const { pad, press } = fake()
    pad.axes = [0.12345, -1, 0, 0.5]
    press(7, true)
    const sample = joySample(pad)
    assertEquals(sample.axes, [0.123, -1, 0, 0.5])
    assertEquals(sample.buttons[7], 1)
    assertEquals(sample.buttons.length, 17)
})
