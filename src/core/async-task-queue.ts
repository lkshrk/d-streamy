export class AsyncTaskQueue {
  private tail: Promise<void> = Promise.resolve();

  add(task: () => Promise<void> | void): Promise<void> {
    const run = this.tail.then(task, task);
    this.tail = run.catch(() => {});
    return run;
  }

  idle(): Promise<void> {
    return this.tail;
  }
}
