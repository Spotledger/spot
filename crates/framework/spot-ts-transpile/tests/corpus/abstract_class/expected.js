export class Handler {
	describe() {
		return `handler:${this.kind()}`;
	}
}
export class MailHandler extends Handler {
	kind() {
		return "mail";
	}
}
