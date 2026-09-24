// Whether the Submit dialog is up. Any page may open it.

class SubmitState {
	open = $state(false);
}

export const submitDialog = new SubmitState();
