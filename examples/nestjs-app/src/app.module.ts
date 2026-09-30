import { Module } from "@nestjs/common";
import { ScheduleModule } from "@nestjs/schedule";
import { AuthGuard } from "./auth.guard.js";
import { LoggingInterceptor } from "./logging.interceptor.js";
import { ObserveModule } from "./observe.js";
import { ReportsTask } from "./reports.task.js";
import { UsersController } from "./users.controller.js";
import { UsersService } from "./users.service.js";

@Module({
  imports: [
    ScheduleModule.forRoot(),
    ObserveModule.forRoot({
      // The bridge does not mint credentials. These are sent on every request and
      // are only checked when the bridge itself was given a matching pair.
      appKey: process.env.OBSERVE_APP_KEY ?? "local-key",
      appSecret: process.env.OBSERVE_APP_SECRET ?? "local-secret",
      serviceId: process.env.OBSERVE_SERVICE_ID ?? "example-api",
      // The whole point of the project: the official SDK, pointed somewhere else.
      // `OBSERVE_ENDPOINT` in the environment overrides this without a code change.
      endpoint: process.env.OBSERVE_ENDPOINT ?? "http://localhost:4319",
      // Flush quickly so the example does not look broken while you wait.
      flushInterval: 1000,
      runtimeMetrics: true,
      forwardLogs: true,
    }),
  ],
  controllers: [UsersController],
  providers: [UsersService, AuthGuard, LoggingInterceptor, ReportsTask],
})
export class AppModule {}
