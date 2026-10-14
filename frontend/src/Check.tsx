import type { ReactNode } from "react"

/** The theme's checkbox, sized for a finger: the whole row toggles it. */
export function Check(
    { ui, checked, onChange, label, children }: {
        ui: string
        checked: boolean
        onChange: (checked: boolean) => void
        label?: string
        children?: ReactNode
    },
) {
    return (
        <label className="dim-check big-check">
            <input
                type="checkbox"
                data-ui={ui}
                aria-label={label}
                checked={checked}
                onChange={(event) => onChange(event.target.checked)}
            />
            <span className="box" />
            {children}
        </label>
    )
}
