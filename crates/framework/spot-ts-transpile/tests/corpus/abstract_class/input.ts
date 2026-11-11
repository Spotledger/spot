export abstract class Handler {
  abstract kind(): string;

  describe(): string {
    return `handler:${this.kind()}`;
  }
}

export class MailHandler extends Handler {
  kind(): string {
    return "mail";
  }
}
