<script lang="ts">
	import PencilIcon from '@lucide/svelte/icons/pencil';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { TreeView, createTreeViewCollection } from '@skeletonlabs/skeleton-svelte';
	import type { GuildChannel, Rule } from '$lib/api/types';
	import ChannelKindIcon from './ChannelKindIcon.svelte';
	import Status from '$lib/components/Status.svelte';
	import { channelTree, type ChannelNode } from './channels';

	interface Props {
		channels: GuildChannel[];
		rules: Rule[];
		/** Whether the viewer may add and edit rules. */
		editable: boolean;
		onpick: (channel: GuildChannel, rule: Rule | null) => void;
	}

	let { channels, rules, editable, onpick }: Props = $props();

	const nodes = $derived(channelTree(channels));
	const ruleOf = $derived(new Map(rules.map((rule) => [rule.channel_id, rule])));

	const collection = $derived(
		createTreeViewCollection<ChannelNode>({
			nodeToValue: (node) => node.id,
			nodeToString: (node) => node.name,
			rootNode: { id: 'root', name: '', kind: 'other', channel: null, children: nodes }
		})
	);

	const expanded = $derived(
		nodes.filter((n) => n.children && n.children.length > 0).map((n) => n.id)
	);
</script>

{#if channels.length === 0}
	<p class="text-sm text-surface-600-400">The bot sees no channels here.</p>
{:else}
	<TreeView {collection} defaultExpandedValue={expanded} selectionMode="single" class="text-sm">
		<TreeView.Tree>
			{#each collection.rootNode.children ?? [] as node, index (node.id)}
				{@render treeNode(node, [index])}
			{/each}
		</TreeView.Tree>
	</TreeView>
{/if}

{#snippet leafContent(node: ChannelNode)}
	{@const rule = ruleOf.get(node.id) ?? null}
	<span class="flex min-w-0 flex-1 items-center gap-2">
		<ChannelKindIcon kind={node.kind} class="size-4 shrink-0 text-surface-600-400" />
		<span class="truncate">{node.name}</span>
		{#if rule}
			<Status
				label={rule.enabled ? 'Watched' : 'Rule off'}
				tone={rule.enabled ? 'success' : 'surface'}
				class="text-sm"
			/>
		{/if}
	</span>
	{#if editable && node.channel}
		<button
			type="button"
			class="btn-icon btn-icon-sm hover:preset-tonal"
			title={rule ? 'Edit rule' : 'Add rule'}
			aria-label={rule ? `Edit the rule for ${node.name}` : `Add a rule for ${node.name}`}
			onclick={(event) => {
				event.stopPropagation();
				onpick(node.channel!, rule);
			}}
		>
			{#if rule}<PencilIcon class="size-3.5" />{:else}<PlusIcon class="size-3.5" />{/if}
		</button>
	{/if}
{/snippet}

{#snippet treeNode(node: ChannelNode, indexPath: number[])}
	<TreeView.NodeProvider value={{ node, indexPath }}>
		{#if node.children && node.children.length > 0}
			<TreeView.Branch>
				<TreeView.BranchControl
					class="flex items-center gap-1 rounded-base px-1 py-1 hover:preset-tonal"
				>
					<TreeView.BranchIndicator />
					{#if node.kind === 'category'}
						<TreeView.BranchText
							class="text-sm font-semibold tracking-wide text-surface-600-400 uppercase"
						>
							{node.name}
						</TreeView.BranchText>
					{:else}
						{@render leafContent(node)}
					{/if}
				</TreeView.BranchControl>
				<TreeView.BranchContent class="ml-4 border-l border-surface-200-800 pl-2">
					{#each node.children as child, childIndex (child.id)}
						{@render treeNode(child, [...indexPath, childIndex])}
					{/each}
				</TreeView.BranchContent>
			</TreeView.Branch>
		{:else if node.kind === 'category'}
			<TreeView.Item class="flex items-center gap-1 rounded-base px-1 py-1">
				<span class="text-sm font-semibold tracking-wide text-surface-600-400 uppercase"
					>{node.name}</span
				>
				<span class="text-sm text-surface-600-400">(empty)</span>
			</TreeView.Item>
		{:else}
			<TreeView.Item class="flex items-center gap-1 rounded-base px-1 py-1 hover:preset-tonal">
				{@render leafContent(node)}
			</TreeView.Item>
		{/if}
	</TreeView.NodeProvider>
{/snippet}
