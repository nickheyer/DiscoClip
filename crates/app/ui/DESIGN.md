# UI and writing standards

## Navigation

- Group pages by task. Use the same term for the same thing throughout the app.
- Use **Discord server** in the interface. Keep `guild` in API fields and routes.
- Use **media site** for a shared clip website. Keep `frontend` in API fields.
- Give long editors named sections and direct navigation.
- Preserve edits when users change sections. Show unsaved state beside save controls.
- Keep the primary action visible. Separate deletion from normal editing.

## Forms

- Put labels above controls. Placeholders are examples, never labels.
- Use one column for unrelated fields. Pair fields only when they form one value or range.
- Group related controls with a fieldset and legend.
- Mark optional fields. Explain what a blank value does when it changes behavior.
- Put units in labels or beside inputs. Choose input widths that fit the expected value.
- Show dependent options only when they apply.
- Keep advanced options in named sections or details controls.
- Use native controls where possible. All actions must work with a keyboard.
- Name the action: **Save profile**, **Create token**, **Download clip**.
- Show validation errors beside fields and link them from a focused error summary.
- Explain how to correct errors. Preserve entered values after a failed request.

## Visual design

- Use the shared colors, spacing and controls in `app.css` and `lib/components`.
- Target 44px for primary controls and at least 24px for compact actions.
- Use at least 4.5:1 contrast for normal text and 3:1 for large text and control boundaries.
- Keep keyboard focus visible. Use text alongside status colors.
- Support narrow screens without shrinking text or hiding required actions.
- Keep tables horizontally scrollable and headers visible.
- Respect reduced-motion preferences.

## Copy

- Write labels and instructions, not narration about how the system works.
- Use familiar words, sentence case and active verbs.
- Limit each sentence to 30 words. Keep hints and page descriptions to two short sentences.
- Remove hints that repeat the label.
- Use **Assign profile**, **Inherited**, **Maximum size** and **Create token**.
- Do not use em dashes, semicolons in sentences or filler phrases.
- Comments explain constraints and decisions. Remove commentary that repeats the code.
- Docs describe the current behavior. Keep API names, units and examples exact.

## Checks

`npm run check` runs the copy rules, TypeScript checks and Svelte accessibility diagnostics.
`npm run build` compiles the production app.

The copy check scans interface text, source comments and project docs. It excludes
code syntax, API identifiers and fenced examples. It rejects prohibited punctuation,
listed filler phrases and sentences over 30 words.

These checks do not replace checking keyboard navigation, contrast and layout in a browser.

## References

- [W3C forms tutorial](https://www.w3.org/WAI/tutorials/forms/): labels, grouping and feedback.
- [GOV.UK text input](https://design-system.service.gov.uk/components/text-input/): labels, hints and input widths.
- [GOV.UK error summary](https://design-system.service.gov.uk/components/error-summary/): focus and links to invalid fields.
