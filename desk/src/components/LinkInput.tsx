import { useEffect, useId, useState } from 'react';

import { listDocuments } from '../api';
import { suggestionParams } from '../links';

interface Props {
  id: string;
  /** The target DocType of the Link field. */
  target: string;
  value: string;
  onChange: (value: string) => void;
  readOnly: boolean;
  required: boolean;
  invalid: boolean;
  describedBy?: string;
  ariaLabel?: string;
}

/** Wait this long after the last keystroke before asking the server for suggestions. */
const DEBOUNCE_MS = 250;

/**
 * A Link field: a text input with the native suggestion list (`<input list>` and `<datalist>`).
 * The suggestions are the target's names that start with what was typed (`suggestionParams`);
 * the user may type any name and the server decides. A target the user cannot read (403) or any
 * other failure simply gives no suggestions.
 */
export function LinkInput({ id, target, value, onChange, readOnly, required, invalid, describedBy, ariaLabel }: Props) {
  const listId = useId();
  const [names, setNames] = useState<string[]>([]);
  const [focused, setFocused] = useState(false);

  useEffect(() => {
    if (readOnly || !focused) return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      listDocuments(target, suggestionParams(value)).then(
        (rows) => {
          if (!cancelled) setNames(rows.map((row) => String(row.name ?? '')).filter((n) => n !== ''));
        },
        () => {
          if (!cancelled) setNames([]);
        },
      );
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [target, value, readOnly, focused]);

  return (
    <>
      <input
        id={id}
        type="text"
        list={readOnly ? undefined : listId}
        value={value}
        readOnly={readOnly}
        aria-required={required || undefined}
        aria-invalid={invalid || undefined}
        aria-describedby={describedBy}
        aria-label={ariaLabel}
        autoComplete="off"
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
      />
      {readOnly ? null : (
        <datalist id={listId}>
          {names.map((name) => (
            <option key={name} value={name} />
          ))}
        </datalist>
      )}
    </>
  );
}
