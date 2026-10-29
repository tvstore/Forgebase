import { useEffect, useRef } from 'react';

interface Props {
  /** The question, also the accessible name of the group. */
  message: string;
  confirmLabel: string;
  cancelLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * An in-page question (no `window.confirm`): focus moves to the safe answer when it appears and
 * returns to where it was when it goes away. Escape answers "no".
 */
export function Prompt({ message, confirmLabel, cancelLabel, onConfirm, onCancel }: Props) {
  const cancelRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const before = document.activeElement;
    cancelRef.current?.focus();
    return () => {
      if (before instanceof HTMLElement && before.isConnected) before.focus();
    };
  }, []);

  return (
    <div
      role="group"
      aria-label={message}
      className="prompt"
      onKeyDown={(e) => {
        if (e.key === 'Escape') onCancel();
      }}
    >
      <p>{message}</p>
      <button type="button" className="danger" onClick={onConfirm}>
        {confirmLabel}
      </button>{' '}
      <button type="button" ref={cancelRef} onClick={onCancel}>
        {cancelLabel}
      </button>
    </div>
  );
}
