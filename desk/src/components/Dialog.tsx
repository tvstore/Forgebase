import { useEffect, useId, useRef } from 'react';
import type { ReactNode } from 'react';

interface Props {
  title: string;
  children?: ReactNode;
  confirmLabel: string;
  cancelLabel: string;
  /** Draws the confirm button as a danger button (a destructive answer). */
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * A modal question (never `window.confirm`), on the native `<dialog>` element opened with
 * `showModal()`: the rest of the page is inert while it is open, Tab stays inside it, and Escape
 * asks to close it (`onCancel`). Focus goes to the safe answer when it opens and returns to where
 * it was when it closes.
 */
export function Dialog({ title, children, confirmLabel, cancelLabel, danger = false, onConfirm, onCancel }: Props) {
  const ref = useRef<HTMLDialogElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const titleId = useId();

  useEffect(() => {
    const before = document.activeElement;
    const dialog = ref.current;
    if (dialog && !dialog.open) dialog.showModal();
    cancelRef.current?.focus();
    return () => {
      dialog?.close();
      if (before instanceof HTMLElement && before.isConnected) before.focus();
    };
  }, []);

  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-labelledby={titleId}
      onCancel={(event) => {
        // Escape: let the page decide (it unmounts this dialog), not the browser.
        event.preventDefault();
        onCancel();
      }}
    >
      <h2 id={titleId}>{title}</h2>
      {children ? <div className="dialog-body">{children}</div> : null}
      <div className="dialog-actions">
        <button type="button" className="btn" ref={cancelRef} onClick={onCancel}>
          {cancelLabel}
        </button>
        <button type="button" className={danger ? 'btn btn-danger' : 'btn btn-primary'} onClick={onConfirm}>
          {confirmLabel}
        </button>
      </div>
    </dialog>
  );
}
