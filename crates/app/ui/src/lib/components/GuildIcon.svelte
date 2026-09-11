<script lang="ts">
	import Avatar from './Avatar.svelte';
	import { guildIconUrl } from '$lib/format';

	let { id, icon, name, size = 32 }: { id: string; icon: string | null; name: string; size?: number } =
		$props();
</script>

<Avatar {name} {size} src={guildIconUrl(id, icon, size >= 48 ? 128 : 64)} />
