import { cx } from '../cx';
import { shownValue } from '../display';
import type { FormValue } from '../fields';
import type { LinkTitles } from '../linkTitles';
import type { FieldMeta } from '../types';

interface Props {
  field: FieldMeta;
  value: FormValue;
  titles: LinkTitles;
}

/**
 * The value of a read-only or locked field as the cell of a table: plain text, numbers on the
 * right, a Link by its title with its key muted after it. The column header names it.
 */
export function ReadOnlyCell({ field, value, titles }: Props) {
  const shown = shownValue(field, value, titles);
  return (
    <td className={cx('cell-readonly', shown.numeric && 'num')}>
      <span className={cx(shown.empty && 'muted', shown.multiline && 'whitespace-pre-wrap')}>{shown.text}</span>
      {shown.key ? <span className="link-key block">{shown.key}</span> : null}
    </td>
  );
}

/**
 * A read-only or locked field of the form: a one-item description list, the label as the term and
 * the value as the description, so that a screen reader reads them together. Not an input: it
 * lines up with the other labels and a number is not pushed around by a spinner.
 */
export function ReadOnlyField({ field, value, titles, className }: Props & { className?: string }) {
  const shown = shownValue(field, value, titles);
  return (
    <dl className={cx('field', 'readonly', className)}>
      <dt>{field.label}</dt>
      <dd className={cx(shown.empty && 'is-empty', shown.multiline && 'is-multiline')}>
        {shown.text}
        {shown.key ? <span className="link-key block">{shown.key}</span> : null}
      </dd>
    </dl>
  );
}
