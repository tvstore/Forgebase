# 0017 Desk styling: Tailwind CSS, theme tokens, no component kits

- Status: Accepted (owner decisions T1 to T4 of the plan "UX.3 Desk restyle with Tailwind CSS", 2026-10-30). Supersedes owner decision UX-5 (the yellow accent) of UX.2.
- Date: 2026-10-30

## Context

UX.2 (PR #26) gave the desk a layout, components and one hand-written stylesheet (`desk/src/styles.css`, about 1,000 lines of plain CSS with custom properties), and the Forgebase yellow as its only accent. The owner asked for a UI that is clean, simple, elegant and suited to corporate use, built with Tailwind, with no yellow and with the background colour of the logo banner (navy `#1B2A5C`, `docs/assets/logo-banner.svg`). The desk's other constraints do not change: no UI component library (M4 decision 1), only `react` and `react-dom` at runtime, a Content-Security-Policy that forbids inline script and style, and a build that needs no network.

## Decision

The decisions below are the owner's.

- **T1. Tailwind CSS v4 as a build-time tool.** `tailwindcss` and `@tailwindcss/vite`, both exact `4.3.3` (MIT), are **development dependencies**: Vite compiles `desk/src/styles.css` into the one static CSS file that the desk already ships under `assets/`. Nothing of Tailwind runs in the browser. Still **no UI component library** and no Tailwind plugin (no typography, forms or container-query plugin, no Tailwind UI, daisyUI, Headless UI or shadcn), and no icon set.
- **T2. A corporate palette.** The primary colour is navy `#1B2A5C`, with a darker hover and two light tints for selected rows and the active sidebar item. Neutrals are slate greys on a light page background. **Yellow stays only inside the logo mark** and is never a UI accent. This supersedes UX-5.
- **T3. The system font stack.** No web font, so no external request (CSP).
- **T4. UX.3 comes before M8.1.** The M8 desk work uses this theme.

How it is built (implementation, not a decision): the tokens are in the `@theme` block of `desk/src/styles.css`; a small `@layer components` holds the classes that would otherwise be long repeated utility lists (`.btn`, `.badge-*`, `.card`, the table, the shell), written with `@apply` and the tokens; components also use utilities directly. There is one style system, not two. Class names that are chosen at run time are full literal strings (`Badge`), because Tailwind finds classes by scanning the source. Contrast ratios are recorded in [architecture, section 11](../blueprint/architecture.md#11-desk-reports-workflow).

## Alternatives considered

- **Keep the hand-written CSS and only change the colours.** The cheapest change, and what the plan weighed first. Not chosen: the owner decided on Tailwind (T1) for a consistent scale and for a styling vocabulary that new desk work in M8 can follow without growing a private one.
- **A component library or a Tailwind plugin kit.** Rejected (M4 decision 1): every runtime dependency is a maintenance cost, and the desk has a small, fixed set of components.
- **Tailwind through the CDN script or the CLI standalone binary.** Rejected: the CDN is a runtime dependency and an external request (CSP); the standalone binary is a download outside npm that the licence and audit checks do not see.

## Consequences

- **One more development toolchain to maintain.** Upgrades of `tailwindcss` and `@tailwindcss/vite` are pinned and go through review with before and after screenshots ([maintenance policy](../blueprint/maintenance-policy.md)).
- **Dependency cost:** 33 more packages in the lockfile, all development only and all MIT, ISC or MPL-2.0 (the allow-list is unchanged); details in [dependency evaluation, section 12](../blueprint/dependency-evaluation.md#12-frontend). `@tailwindcss/oxide` is a native binary, chosen by npm from its optional platform packages; `desk/.npmrc` still sets `ignore-scripts=true` and no install script is needed.
- **The CSP is unchanged.** The built `index.html` has no inline script or style, and the CSS has no `@import` and no `url(...)`.
- **The shipped CSS is generated.** Review the source (`styles.css` and the components), not `dist/`. The CSS may use modern features (`color-mix`, cascade layers), which the supported browsers have.
- The yellow of UX.2 is gone from the UI; the logo mark keeps it. Checks for that: a search of the sources for the two yellow values finds only `Logo.tsx` and the logo asset.
