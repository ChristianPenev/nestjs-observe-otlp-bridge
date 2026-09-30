import { Injectable, Logger } from "@nestjs/common";
import { Interval } from "@nestjs/schedule";

@Injectable()
export class ReportsTask {
  private readonly logger = new Logger(ReportsTask.name);

  // Fires on a timer rather than off a queue, so it exports as a scheduled task
  // rather than as messaging traffic.
  @Interval("rollup", 15_000)
  async rollup() {
    await new Promise((resolve) => setTimeout(resolve, 20));
    this.logger.log("rolled up yesterday's figures");
  }
}
