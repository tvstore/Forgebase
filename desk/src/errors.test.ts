import { describe, expect, it } from 'vitest';

import { mapDetail, mapDetails } from './errors';
import { errorKey } from './fields';
import { field, meta } from './testing';

const order = meta([
  field('customer', 'link', { target: 'customer', required: true }),
  field('credit', 'decimal'),
  field('lines', 'table', {
    target: 'line',
    required: true,
    fields: [field('item', 'data'), field('qty', 'int')],
  }),
]);

describe('error details to fields', () => {
  it('maps a field problem', () => {
    expect(mapDetail('field `customer` is required', order)).toEqual({
      key: 'customer',
      message: 'This field is required.',
    });
    expect(mapDetail('field `credit`: 3 decimal places exceeds scale 2', order)).toEqual({
      key: 'credit',
      message: '3 decimal places exceeds scale 2.',
    });
    expect(mapDetail('field `customer`: customer `X` does not exist', order)).toEqual({
      key: 'customer',
      message: 'Customer `X` does not exist.',
    });
  });

  it('maps a cell of a table, in both wordings the API uses', () => {
    expect(mapDetail('field `lines` row 2: field `qty` is required', order)).toEqual({
      key: errorKey('lines', 2, 'qty'),
      message: 'This field is required.',
    });
    expect(mapDetail('field `lines`: row 3: field `qty`: expected an integer, found a string', order)).toEqual({
      key: errorKey('lines', 3, 'qty'),
      message: 'Expected an integer, found a string.',
    });
  });

  it('maps a table problem that is not about a cell to the table', () => {
    expect(mapDetail('field `lines` is required', order)).toEqual({
      key: 'lines',
      message: 'This field is required.',
    });
    expect(mapDetail('field `lines` row 1: unknown field `zzz`', order)?.key).toBe('lines');
  });

  it('leaves what names no field of the form unmapped', () => {
    expect(mapDetail('unknown field `nope`', order)).toBeNull();
    expect(mapDetail('field `nope` is required', order)).toBeNull();
    expect(mapDetail('the given name differs from the value of naming field `x`', order)).toBeNull();
  });

  it('splits a list: the first message per field wins, the rest is unmapped', () => {
    const mapped = mapDetails(
      [
        'field `customer` is required',
        'field `customer`: customer `X` does not exist',
        'field `lines` row 1: field `item` is required',
        'unknown field `nope`',
      ],
      order,
    );
    expect(mapped.errors).toEqual({
      customer: 'This field is required.',
      [errorKey('lines', 1, 'item')]: 'This field is required.',
    });
    expect(mapped.unmapped).toEqual(['unknown field `nope`']);
  });
});
