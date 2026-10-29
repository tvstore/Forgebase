import { useState } from 'react';
import type { FormEvent } from 'react';

import { NO_VALUE_OPS, buildFilters, opsFor } from '../filters';
import type { FilterField, FilterRow, Op } from '../filters';

interface Props {
  fields: FilterField[];
  /** The rows of the address. The parent changes `key` when they change. */
  initial: FilterRow[];
  /** Called with the `filters` JSON text, or null for no filter. */
  onApply: (filters: string | null) => void;
}

/** A yes/no field always shows a choice, so it starts with one. */
const defaultValue = (field: FilterField): string => (field.type === 'check' ? 'true' : '');

function newRow(fields: FilterField[]): FilterRow | null {
  const field = fields[0];
  const op = field ? opsFor(field.type)[0] : undefined;
  return field && op ? { field: field.name, op, value: defaultValue(field) } : null;
}

/** A filter bar: add and remove rows of field, operator and value; Apply builds the JSON. */
export function FilterBar({ fields, initial, onApply }: Props) {
  const [rows, setRows] = useState<FilterRow[]>(initial);
  const [error, setError] = useState<string | null>(null);

  const change = (index: number, patch: Partial<FilterRow>) =>
    setRows((current) => current.map((row, i) => (i === index ? { ...row, ...patch } : row)));

  const changeField = (index: number, name: string) => {
    const field = fields.find((f) => f.name === name);
    const op = field ? opsFor(field.type)[0] : undefined;
    if (field && op) change(index, { field: name, op, value: defaultValue(field) });
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const built = buildFilters(rows, fields);
    if (!built.ok) {
      setError(built.error);
      return;
    }
    setError(null);
    onApply(built.filters.length === 0 ? null : JSON.stringify(built.filters));
  };

  const add = () => {
    const row = newRow(fields);
    if (row) setRows((current) => [...current, row]);
  };

  return (
    <form className="filters" onSubmit={submit}>
      <fieldset>
        <legend>Filters</legend>
        {rows.map((row, index) => {
          const field = fields.find((f) => f.name === row.field);
          const number = index + 1;
          return (
            <div className="filter-row" key={index}>
              <select
                aria-label={`Filter ${number} field`}
                value={row.field}
                onChange={(e) => changeField(index, e.target.value)}
              >
                {fields.map((f) => (
                  <option key={f.name} value={f.name}>
                    {f.label}
                  </option>
                ))}
              </select>
              <select
                aria-label={`Filter ${number} operator`}
                value={row.op}
                onChange={(e) =>
                  change(index, {
                    op: e.target.value as Op,
                    value: row.value || (field ? defaultValue(field) : ''),
                  })
                }
              >
                {(field ? opsFor(field.type) : []).map((op) => (
                  <option key={op} value={op}>
                    {op}
                  </option>
                ))}
              </select>
              {NO_VALUE_OPS.includes(row.op) ? null : (
                <ValueInput
                  field={field}
                  row={row}
                  label={`Filter ${number} value`}
                  onChange={(value) => change(index, { value })}
                />
              )}
              <button
                type="button"
                aria-label={`Remove filter ${number}`}
                onClick={() => setRows((current) => current.filter((_, i) => i !== index))}
              >
                Remove
              </button>
            </div>
          );
        })}
        <div className="filter-actions">
          <button type="button" onClick={add}>
            Add filter
          </button>
          <button type="submit" className="primary">
            Apply filters
          </button>
        </div>
        {error ? (
          <p role="alert" className="error">
            {error}
          </p>
        ) : null}
      </fieldset>
    </form>
  );
}

interface ValueProps {
  field: FilterField | undefined;
  row: FilterRow;
  label: string;
  onChange: (value: string) => void;
}

function ValueInput({ field, row, label, onChange }: ValueProps) {
  const single = row.op === '=' || row.op === '!=';
  if (field?.type === 'check') {
    return (
      <select aria-label={label} value={row.value} onChange={(e) => onChange(e.target.value)}>
        <option value="true">Yes</option>
        <option value="false">No</option>
      </select>
    );
  }
  if (field?.type === 'select' && field.options && single) {
    return (
      <select aria-label={label} value={row.value} onChange={(e) => onChange(e.target.value)}>
        <option value="">(choose)</option>
        {field.options.map((option) => (
          <option key={option} value={option}>
            {option}
          </option>
        ))}
      </select>
    );
  }
  const type = field?.type === 'date' && row.op !== 'in' ? 'date' : 'text';
  return (
    <input
      aria-label={label}
      type={type}
      value={row.value}
      placeholder={row.op === 'in' ? 'a, b, c' : row.op === 'like' ? 'pattern, % matches any text' : ''}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}
