import React from "react";

/**
 * One choice out of a few, as a pill track with the chosen option filled.
 * Shared by every multiple-choice toggle in the app, so they read as one control.
 */
export function Segmented<T extends string | number>({
  options,
  value,
  onChange,
  label,
}: {
  options: readonly T[];
  value: T;
  onChange: (option: T) => void;
  label: (option: T) => string;
}) {
  return (
    <div className="inline-flex shrink-0 items-center gap-0.5 rounded-md bg-mid-gray/15 p-0.5">
      {options.map((option) => {
        const selected = option === value;
        return (
          <button
            key={String(option)}
            type="button"
            aria-pressed={selected}
            onClick={() => onChange(option)}
            className={`h-6 min-w-[2.75rem] rounded px-2 text-xs leading-none transition-colors cursor-pointer ${
              selected
                ? // The brand pink is light in both themes; dark text keeps the
                  // chosen label readable on it.
                  "bg-logo-primary font-medium text-neutral-900"
                : "text-text/55 hover:bg-mid-gray/20 hover:text-text"
            }`}
          >
            {label(option)}
          </button>
        );
      })}
    </div>
  );
}
