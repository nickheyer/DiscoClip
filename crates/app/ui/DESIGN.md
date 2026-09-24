# DiscoClip web app design

The web app lives in `crates/app/ui`. It is a SvelteKit single-page app on Svelte 5,
Tailwind 4 and Skeleton 5, built by `crates/app/build.rs` and embedded in the server binary.
This file is the system every page follows.

## Stack

| Piece      | Choice                                                                                                                                                                              |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Framework  | SvelteKit 2 on Svelte 5 runes, `ssr = false`, adapter-static with an `index.html` fallback                                                                                          |
| Styling    | Tailwind 4 through `@tailwindcss/vite`, with `@tailwindcss/forms` for Skeleton's native form controls                                                                               |
| Components | Skeleton 5: `@skeletonlabs/skeleton` for the Tailwind layer, `@skeletonlabs/skeleton-svelte` for Dialog, Menu, Navigation, AppBar, Toast, Combobox, TreeView, TagsInput, Pagination |
| Icons      | `@lucide/svelte`, imported one icon at a time from `@lucide/svelte/icons/<name>`                                                                                                    |
| Fonts      | Inter Variable for text, JetBrains Mono Variable for identifiers, links and logs. Both ship in the bundle because the server's CSP allows `font-src 'self'` only                    |

## Brand

The mark is the clipped disc: a solid circle with a 60° wedge cut from its upper right, and the
wedge drawn again shifted outward along its own bisector by 14% of the diameter.
`src/lib/brand/Mark.svelte` draws it in `currentColor`; `Wordmark.svelte` sets it in
primary-500 beside "DiscoClip" in Inter 700 with `-0.03em` tracking. The SVG sources and a
512 px avatar live in `brand/` at the repository root; the favicon and PWA icons in `static/`.

## Palette

The theme is `discoclip` in `src/lib/brand/theme.css`, generated from these anchors. Every
colour is an OKLCH scale from 50 to 950 with Skeleton's contrast tokens.

| Token     | Anchor                           | Use                                                            |
| --------- | -------------------------------- | -------------------------------------------------------------- |
| primary   | `#f2542d` coral                  | The page's main action, the active navigation accent, the mark |
| secondary | `#2ec4b6` teal                   | Informational states, Discord links, config-sourced values     |
| tertiary  | `#f5b700` amber                  | Reserved for highlights                                        |
| success   | `#22c55e`                        | Done, connected, passing                                       |
| warning   | `#f59e0b`                        | Retrying, cancelled, login required, disabled views            |
| error     | `#ef4444`                        | Failed, destructive actions, live markers                      |
| surface   | `#f7f7f8` to `#0b0b0e` cool grey | Backgrounds, borders, muted text                               |

Primary is rationed. Only three things wear it: the one filled button that is the page's main
action, the 3px accent and icon on the active navigation entry, and the brand mark. Links in
tables and key-value lists take body colour instead, so orange always means "this is the thing
to press here".

Light mode sits on surface-50, dark mode on surface-950. The mode follows the system until the
person switches it from the account menu; the choice is kept in
`localStorage['discoclip.mode']` and applied by the inline script in `app.html` before paint. Use Skeleton's paired classes
(`bg-surface-100-900`, `text-surface-600-400`) so both modes come from one class.

## Type, spacing, radius

- The type scale is set in `theme.css` as the Tailwind `--text-*` tokens: `text-xs` 14px,
  `text-sm` 16px, `text-base` 18px, `text-lg` 20px, `text-xl` 22px, `text-2xl` 26px,
  `text-3xl` 32px. Body text is `text-base`, Inter, weight 400. Headings use weight 650 with
  `-0.02em` tracking through Skeleton's heading classes. The breadcrumb uses 26px on small
  screens and 32px from `sm`. The page title shows from `lg` at 32px; below that the breadcrumb
  alone names the page. Section titles use 20px.
- Primary content reads at body size: table cells and headers, navigation entries, menu
  items, form controls and buttons. `app.css` sets Skeleton's `table` to
  `text-base` so every table follows without a class.
- Secondary text is `text-sm`: descriptions, help text, captions, group labels, field labels,
  status lines and flags. `app.css` sets Skeleton's `label-text` to that size.
- 16px is the floor for text in the sans family. Nothing outside `font-mono` goes below it.
  `text-xs` is only for the mono family: identifiers, URLs, keys, tokens and log lines.
- Spacing is Tailwind's 4px scale. Panels use 20px padding, increasing to 24px from `sm`.
  Page sections use a 24px gap, increasing to 32px on desktop. Panels use a subtle
  `border-surface-200-800` border to separate them from the page background.
- Standard buttons and single-line fields have a minimum height of 44px. Compact row
  actions use 36px. Icon buttons use the same minimum width and height.
- `--radius-base` is 6px (buttons, inputs); `--radius-container` is 12px (cards, dialogs,
  tables).

## Layout

- The operator shell in `src/routes/(app)/+layout.svelte` uses Skeleton Navigation in
  `sidebar` mode from `lg`. The 256px sidebar groups destinations into Workspace,
  Configuration, Administration, and Monitoring. Its links scroll independently when
  needed; the brand and live connection status stay visible.
- Below `lg`, a bottom bar provides Dashboard, Jobs, Profiles, and More. More and the
  AppBar menu button open the same Skeleton Dialog drawer with all navigation groups.
  The bar, drawer footer, and page clearance account for the device's safe area.
- The active navigation entry is a neutral highlight, never a coloured block: background
  `bg-surface-200-800`, label `text-surface-950-50` at weight 600, icon in primary-500, and a
  3px primary left border in the sidebar and drawer. Inactive entries carry a transparent 3px
  border so the label never shifts. The bottom bar uses the same background and icon without
  the border.
- The AppBar is 64px high on small screens and 80px on desktop. Padding belongs to its
  toolbar only. It holds exactly two things on every page: the breadcrumb on the left, at
  page-title size, and the account menu on the right. Nothing else goes in it. The breadcrumb
  reads Group, then the section entry as a link, then the page on show with `aria-current`.
  The group comes from the navigation entry whose href is the longest match for the path;
  a page outside the navigation shows its own name alone.
- The account menu opens with a row holding the name and role on the left and the Account
  button on the right, then the light or dark mode switch, then Log out.
- A page's actions never sit in its header. They sit in a `Toolbar`, right-aligned and
  directly above the block they act on, so a list's New button sits on the top-right corner
  of its table. `Toolbar` pulls the block after it up to a 12px gap. The page's description
  is the toolbar's `description` and opens the same row from `lg`; below that it is hidden,
  and a toolbar holding nothing else is hidden with it. A line of context, such as a count
  or a refresh time, goes before the actions with `mr-auto`.
- Operator pages fill the available column with `minmax(0, 1fr)` and use 16px, 24px,
  then 32px gutters. `main` is centered with `mx-auto w-full max-w-[100rem]`, so content stops
  growing at 1600px and sits in the middle of wider displays.
  Tables keep horizontal overflow inside their wrapper and use normal page scrolling.
- The dashboard uses two, three, or six statistic columns and a wide active-job panel
  beside a 352px column for applications and health at `xl`. Loading placeholders are
  distinct from empty results. Queued and running jobs both load into the active list.
- Dialogs use Skeleton's headless Dialog and Portal with explicit overlay layers. Headers
  and footers stay visible while the body scrolls. Forms use full-screen dialogs on phones.
- Auth pages (`(auth)`) are a centered card under the wordmark. Public views (`(front)`)
  have their own slim header and no operator navigation.

## Components in use

| Component                        | For                                                                                                           |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `PageHeader`                     | Every page's title, shown from `lg`, and the meta line under it. It also sets the tab title                   |
| `DataTable`                      | Every list: columns with snippets, sorting, selection, placeholders, empty states, and an Edit button per row |
| `Status`                         | Job, bot, health, fixture, session and on/off states as a coloured dot and text                               |
| `Toolbar`                        | A page's description and actions, right-aligned on the top corner of the block below                          |
| `Modal`, `Confirm`               | Forms in dialogs and yes-or-no questions. Destructive confirms are red                                        |
| `EmptyState`, `ErrorState`       | What a list shows when it has nothing or could not load                                                       |
| `Field`                          | A label, control, help text and error message                                                                 |
| `RelativeTime`, `Bytes`, `Clock` | Numbers people read: times relative with the absolute time on hover                                           |
| `SearchInput`, `Pager`           | Filtering and offset paging                                                                                   |
| `CopyButton`                     | Anything someone pastes elsewhere: ids, links, tokens                                                         |
| `StatTile`                       | Dashboard and metrics numbers                                                                                 |

Buttons: `preset-filled-primary-500` for the one main action on a page, `preset-filled` for
secondary submits, `preset-tonal` for everything else, `preset-tonal-error` for destructive
actions. Icon buttons are `btn-icon` with `hover:preset-tonal`. A page has one filled primary
button, and it sits in the page's `Toolbar`.

There are no badges, chips or pills. A state is `Status`: a coloured dot and plain text in the
tone's 700-300 colour. Flags, tags, counts and options are plain text, muted where they are
secondary, separated by middle dots when several share a line.

Links inside tables and key-value lists use `link-body`: body colour, no underline until hover,
and any trailing external-link icon in `text-surface-600-400`. `DataTable`'s Edit button is a
`btn-sm preset-tonal` link labelled Edit, or View for rows that open a read-only page.

Use Skeleton components through their composed parts and style the part that owns the
property. For example, text size belongs on `Navigation.TriggerText`, and overlay stacking
belongs on `Menu.Positioner`. Use native links for table navigation so keyboard access,
copying links, and opening new tabs work normally.

The implementation follows the official [Skeleton v5 Svelte reference](https://www.skeleton.dev/llms-svelte.txt),
particularly [Navigation](https://www.skeleton.dev/docs/svelte/framework-components/navigation),
[Layouts](https://www.skeleton.dev/docs/svelte/guides/layouts), and
[Forms](https://www.skeleton.dev/docs/svelte/tailwind-components/forms).

## Behaviour

- One typed client (`src/lib/api/client.ts`) and one function per endpoint
  (`src/lib/api/endpoints.ts`). Nothing else calls `fetch`.
- One `EventSource` per browser (`src/lib/events.svelte.ts`), elected with a Web Lock and
  shared over a BroadcastChannel. Pages subscribe to job events and read `feed.stats` and
  `feed.bots`.
- Every mutating button disables while its request is in flight and shows a `Spinner`.
- Every list shows placeholder rows while loading, an `EmptyState` when empty and an
  `ErrorState` with Retry on failure.
- Internal links and `goto` calls use `resolve()` from `$app/paths`. Parameterised routes use
  their full id including the group, such as `/(app)/jobs/[id]`.
- Filters that people share live in the URL query (jobs).
- A `401` from any request ends the session and returns to `/login?next=`.

## Writing

- Sentences, not fragments. One idea per sentence. Say what happens, not what the code does.
- Labels are nouns ("Max height"), buttons are verbs ("Set as default", "End all").
- Empty states say what would fill them ("Media appears here once jobs finish").
- Errors say what went wrong and, where it helps, what to do ("Enter seconds or h:mm:ss").
- Confirmations name the thing and its consequence ("Delete Clips? Its accounts and viewer
  sessions go with it.").
- Times are relative ("3 minutes ago") with the absolute time on hover.
- An absent time reads "Never". Every other absent value reads "None". Both come from `EMPTY`
  and `NEVER` in `src/lib/format.ts`, in `text-surface-600-400`. Dashes never stand for
  absence.
- State labels capitalise their first word, like every other label ("On", "Pass",
  "Login optional").
- Sizes use binary units (`1.5 MB`); durations use `h:mm:ss`.
- Capitalise the first word only. Product names keep their own casing (DiscoClip, Discord).
- No exclamation marks, no jargon a first-time operator would not know.
