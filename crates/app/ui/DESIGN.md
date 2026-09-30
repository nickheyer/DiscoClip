# DiscoClip web app design

The web app lives in `crates/app/ui`. It is a SvelteKit single-page app on Svelte 5,
Tailwind 4 and Skeleton 5, built by `crates/app/build.rs` and embedded in the server binary.
This file is the system every page follows.

## Stack

| Piece      | Choice                                                                                                                                                     |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Framework  | SvelteKit 2 on Svelte 5 runes, `ssr = false`, adapter-static with an `index.html` fallback                                                                 |
| Styling    | Tailwind 4 through `@tailwindcss/vite`, with `@tailwindcss/forms` for Skeleton's native form controls                                                      |
| Components | Skeleton 5: `@skeletonlabs/skeleton` for the Tailwind components and presets, `@skeletonlabs/skeleton-svelte` for the framework components listed below    |
| Icons      | `@lucide/svelte`, imported one icon at a time from `@lucide/svelte/icons/<name>`                                                                           |
| Fonts      | Inter Variable for text, JetBrains Mono Variable for identifiers, keys and logs. Both ship in the bundle because the server's CSP allows `font-src 'self'` |

The rule of the design is that Skeleton's already-styled parts do the styling. A page composes
Skeleton's Tailwind components (`card`, `btn`, `badge`, `chip`, `table`, `input`, `select`,
`switch`, `hr`, `placeholder`, `pre`, `anchor`, the `h1` to `h6` classes and the `preset-*`
presets) and its Svelte components (AppBar, Navigation, Dialog, Menu, Tabs, Accordion,
Collapsible, Combobox, Listbox, TagsInput, Switch, SegmentedControl, Steps, Progress, Avatar,
Pagination, FileUpload, Toast). `app.css` adds no component styles of its own: it
imports Tailwind, Skeleton, the theme and the fonts, sets the two font families, the focus
ring and the reduced-motion rule, and nothing else. There is no `<style>` block anywhere.

## Brand

The mark is the clipped disc: a solid circle with a 60° wedge cut from its upper right, and the
wedge drawn again shifted outward along its own bisector by 14% of the diameter.
`src/lib/brand/Mark.svelte` draws it in `currentColor`; `Wordmark.svelte` sets it in
primary-500 beside "DiscoClip" in Inter 700 with `-0.03em` tracking. The SVG sources and a
512 px avatar live in `brand/` at the repository root; the favicon and PWA icons in `static/`.

## Palette and type

The theme is `discoclip` in `src/lib/brand/theme.css`, generated from these anchors. Every
colour is an OKLCH scale from 50 to 950 with Skeleton's contrast tokens.

| Token     | Anchor                           | Use                                                           |
| --------- | -------------------------------- | ------------------------------------------------------------- |
| primary   | `#f2542d` coral                  | The page's main action, the active navigation entry, the mark |
| secondary | `#2ec4b6` teal                   | Informational states, such as an inherited platform           |
| tertiary  | `#f5b700` amber                  | Reserved for highlights                                       |
| success   | `#22c55e`                        | Done, connected, passing, on                                  |
| warning   | `#f59e0b`                        | Retrying, cancelled, login required, disabled views, secrets  |
| error     | `#ef4444`                        | Failed, destructive actions, live markers                     |
| surface   | `#f7f7f8` to `#0b0b0e` cool grey | Backgrounds, cards, borders, muted text                       |

The type scale is Skeleton's own: the theme sets `--text-scaling: 1` and leaves the sizes to
Skeleton, so body text is 16px and every component is the size Skeleton designed it at.
Headings use the theme's weight 650 with `-0.02em` tracking through the `h1` to `h6` classes.
Page titles are `h3`, card titles `h6`, dialog titles `h5`, auth page titles `h4`.

Light mode sits on surface-50, dark mode on surface-950. The mode follows the system until the
person switches it with the Switch in the app bar; the choice is kept in
`localStorage['discoclip.mode']` and applied by the inline script in `app.html` before paint.
Colours use Skeleton's paired classes (`bg-surface-100-900`, `text-surface-600-400`) so both
modes come from one class. Muted text is `text-surface-600-400`.

Primary is rationed. Only three things wear it: the one filled button that is the page's main
action, the active navigation entry's tonal tint and icon, and the brand mark. Links use the
`anchor` class, which the theme colours primary-700 in light mode and primary-400 in dark.

## Layout

- The operator shell in `src/routes/(app)/+layout.svelte` is Skeleton Navigation in
  `sidebar` layout from `lg`, with Skeleton's own width, padding and trigger styling. Its
  header holds the wordmark, its content the four groups (Workspace, Configuration,
  Administration, Monitoring) as `Navigation.Group`s with a `Navigation.Label` each, and its
  footer the live-updates `Status` badge. The active entry gets `preset-tonal-primary` and its
  icon `text-primary-500`.
- Below `lg`, Skeleton Navigation in `bar` layout is fixed to the bottom with Dashboard, Jobs,
  Profiles and More. More and the app bar's menu button open the same Skeleton Dialog, used
  as a drawer, holding the sidebar Navigation with every group plus Account.
- The Skeleton AppBar is sticky at the top. Its toolbar holds the drawer button (below `lg`),
  the breadcrumb, and a trail with the light switch (a Skeleton Switch with sun and moon on
  the thumb) and the account Menu (the name and role as a group label, Account, Log out). The
  breadcrumb reads Group, then the section entry as an `anchor`, then the page on show.
- A page's actions sit in its `PageHeader`, on the right of the title. The one filled primary
  button goes last. A long form (a profile, a view) has its Cancel and Save in one action bar
  that sticks to the bottom instead, so they appear once and stay reachable.
- `main` is `mx-auto w-full max-w-[100rem]` with a `gap-6` column and 16px, 24px, then 32px
  gutters, so content stops growing at 1600px and sits in the middle of wider displays.
- Every block of content is a `Card`: Skeleton's `card` with `preset-filled-surface-100-900`,
  a `p-4` body, and an optional header with the title as `h6`, a count badge, a description
  and actions. `flush` drops the body padding for a table or a list that runs edge to edge.
- Auth pages (`(auth)`) are a centered card under the wordmark. Public views (`(front)`)
  have their own AppBar and no operator navigation.

## Components in use

Skeleton parts, and what each is for:

| Skeleton part                       | For                                                                                                             |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `card` + presets                    | Every panel, stat tile, filter form, banner and dialog surface. Anchors that are cards get hover styling        |
| `badge` + `preset-tonal-*`          | Every state and count. `Status` renders one; the size is `--badge-size: var(--text-xs)`                         |
| `chip`                              | Removable picks (members, channels) and filter picks (presets)                                                  |
| `btn`, `btn-icon`, `btn-sm`         | Every button. See the presets below                                                                             |
| `table`, `table-wrap`               | Every list, through `DataTable`, with `[&>tr]:hover:preset-tonal` on the body                                   |
| `input`, `select`, `textarea`       | Every text control, through `Field` for the label, help and error                                               |
| `checkbox`, `radio`                 | Multiple picks in a short list, and the rows of a table that pick: a view's scope, command servers              |
| `fieldset`, `legend`, `field-group` | Grouped controls, and an input with a unit or a button beside it                                                |
| `placeholder`                       | Loading blocks, with `animate-pulse`                                                                            |
| `hr`                                | Dividers, including the "or" between login methods                                                              |
| `pre`                               | Logs and JSON, through `CodeBlock`                                                                              |
| `disclosure`                        | A log line that opens to show its fields                                                                        |
| `meter`                             | Disk use on the metrics page                                                                                    |
| AppBar                              | The operator app bar and the public view header                                                                 |
| Navigation                          | The sidebar, the bottom bar and the drawer's contents                                                           |
| Dialog                              | `Modal`, `Confirm` and the navigation drawer, styled as cards                                                   |
| Menu                                | The account menu, the job download menu and the settings export menu                                            |
| Tabs                                | Applications and watch rules, the users page, the account page                                                  |
| Accordion                           | Settings sections, audit entries, role descriptions, a job's description and variants                           |
| Collapsible                         | The submit dialog's options                                                                                     |
| Switch                              | Every on/off value: watching a channel, enabled flags, downloads, links, booleans in settings, the light switch |
| SegmentedControl                    | One of a few short choices: cookie format, platform access, who gets into a view                                |
| Listbox                             | Picking several from a list with search: the roles a rule allows                                                |
| Combobox                            | Searching a server's members                                                                                    |
| TagsInput                           | Discord user ids on a view                                                                                      |
| Steps                               | A job's five stages, and the three phases of a restore                                                          |
| Progress                            | Job progress bars, and `Spinner` as the circular indeterminate form                                             |
| Avatar                              | `GuildIcon` and `DiscordAvatar`, with initials as the fallback                                                  |
| Pagination                          | `Pager` under the jobs table                                                                                    |
| FileUpload                          | Choosing a cookies file or a settings file to import                                                            |
| Toast                               | The one `Toast.Group` in the root layout; pages speak through `notify` and `reportError`                        |

The app's own components in `src/lib/components` compose those parts: `Card`, `PageHeader`,
`Status`, `StatTile`, `DataTable`, `Pager`, `SearchInput`, `Field`, `Modal`, `Confirm`,
`EmptyState`, `ErrorState`, `KeyValue` and `KeyValueRow`, `Identifier`, `CopyButton`,
`CodeBlock`, `Spinner`, `RelativeTime`, `Bytes`, `Clock`, `MediaKindIcon`, `JobTitle`,
`PlaceLine`, `PlatformSummary`, `GuildIcon`, `DiscordAvatar`, `ModeToggle`, `SubmitDialog`,
`CookiesDialog`, `SettingField`, and the guild set (`ChannelTable`, `RuleDialog`,
`MemberProfiles`, `MemberPicker`, `ChannelKindIcon`, `ScopePicker`, with `watching.ts` for
what the switches do).
`ChannelTable` is one table of a server's channels under their category headings, each row
carrying a switch that watches the channel, an Options button on a watched one, and a select
for its profile. With the server watched whole, every row is on unless switched off.
`ScopePicker` is one table of the servers the bots are in, each opening to its channels, with
a checkbox on every row: ticking a server takes every channel in it, and unticking channels
narrows it to the rest.

Buttons: `preset-filled-primary-500` for the one main action on a page or in a dialog,
`preset-filled` for a secondary submit, `preset-tonal` for everything else,
`preset-tonal-error` for destructive actions, `preset-filled-error-500` to confirm a
destructive dialog. Icon buttons are `btn-icon hover:preset-tonal`; row actions are `btn-sm`.
Every mutating button disables while its request is in flight and shows a `Spinner`.

Identifiers, keys, hosts and URLs are `font-mono text-xs`. Times are `RelativeTime` with the
absolute time on hover. Sizes go through `Bytes`, durations through `Clock`.

Use Skeleton components through their composed parts and style the part that owns the
property: a Menu's stacking on `Menu.Positioner`, a Progress bar's height on `Progress.Track`.
Use native links for table navigation so keyboard access, copying links, and opening new tabs
work normally.

The implementation follows the official [Skeleton v5 Svelte reference](https://www.skeleton.dev/llms-svelte.txt).

## Behaviour

- One typed client (`src/lib/api/client.ts`) and one function per endpoint
  (`src/lib/api/endpoints.ts`). Nothing else calls `fetch`.
- One `EventSource` per browser (`src/lib/events.svelte.ts`), elected with a Web Lock and
  shared over a BroadcastChannel. Pages subscribe to job events and read `feed.stats` and
  `feed.bots`.
- Every list shows placeholder rows while loading, an `EmptyState` when empty and an
  `ErrorState` with Retry on failure.
- Each fact appears once on a page. The `PageHeader` line carries a record's state and
  identifiers; cards below it hold what the header does not say. A count sits on the tab or
  the card, not both. Rows that share a shape share one table, so a job in flight and a job
  finished sit in the same table on the dashboard, and a health check is one row of one
  table.
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
