import { describe, expect, it } from 'vitest';

import { formHref, listHref, newHref, parseHash } from './router';

describe('parseHash', () => {
  it('parses the home and login routes', () => {
    for (const hash of ['', '#', '#/', '/']) expect(parseHash(hash)).toEqual({ name: 'home' });
    expect(parseHash('#/login')).toEqual({ name: 'login' });
  });

  it('parses a list with its query', () => {
    const route = parseHash('#/list/customer?offset=20&sort=name%3Adesc');
    expect(route.name).toBe('list');
    if (route.name !== 'list') return;
    expect(route.doctype).toBe('customer');
    expect(route.params.get('offset')).toBe('20');
    expect(route.params.get('sort')).toBe('name:desc');
  });

  it('parses a list without a query and with an encoded name', () => {
    const route = parseHash('#/list/sales%20order');
    expect(route.name === 'list' && route.doctype).toBe('sales order');
    if (route.name === 'list') expect([...route.params]).toEqual([]);
  });

  it('keeps a question mark inside the query', () => {
    const route = parseHash('#/list/a?filters=%5B%5D&x=1?2');
    if (route.name !== 'list') throw new Error('not a list');
    expect(route.params.get('x')).toBe('1?2');
  });

  it('parses new and form routes', () => {
    expect(parseHash('#/new/customer')).toEqual({ name: 'new', doctype: 'customer' });
    expect(parseHash('#/form/customer/ann%40example.com')).toEqual({
      name: 'form',
      doctype: 'customer',
      docName: 'ann@example.com',
    });
    // A name with a slash survives the round trip.
    expect(parseHash(formHref('note', 'a/b'))).toEqual({ name: 'form', doctype: 'note', docName: 'a/b' });
  });

  it('gives notfound for anything else', () => {
    for (const hash of [
      '#/nothing',
      '#nothing',
      '#/list',
      '#/list/',
      '#/list/a/b',
      '#/form/a',
      '#/form/a/b/c',
      '#/new',
      '#/login/x',
      '#/list/%E0%A4%A',
      '#/form/a/%',
    ]) {
      expect(parseHash(hash), hash).toEqual({ name: 'notfound' });
    }
  });
});

describe('hrefs', () => {
  it('encode names and round-trip through parseHash', () => {
    expect(listHref('customer')).toBe('#/list/customer');
    expect(listHref('a b', new URLSearchParams({ sort: 'x:desc' }))).toBe('#/list/a%20b?sort=x%3Adesc');
    expect(listHref('x', new URLSearchParams())).toBe('#/list/x');
    expect(newHref('a/b')).toBe('#/new/a%2Fb');
    expect(parseHash(newHref('a/b'))).toEqual({ name: 'new', doctype: 'a/b' });
    const route = parseHash(listHref('a b', new URLSearchParams({ filters: '[{"field":"x"}]' })));
    if (route.name !== 'list') throw new Error('not a list');
    expect(route.params.get('filters')).toBe('[{"field":"x"}]');
  });
});
