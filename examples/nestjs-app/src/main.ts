import "reflect-metadata";
import { NestFactory } from "@nestjs/core";
import { AppModule } from "./app.module.js";
import { ObserveInstrument } from "./observe.js";

async function bootstrap() {
  const app = await NestFactory.create(AppModule, {
    // Without this the agent reports requests with no spans underneath them, and
    // says so loudly in the log.
    instrument: ObserveInstrument,
  });
  await app.listen(Number(process.env.PORT ?? 3000));
  console.log(`example app listening on ${await app.getUrl()}`);
}

void bootstrap();
