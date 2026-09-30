import { createObserveModule } from "@nestjs/observe";

// `createObserveModule()` hands back both halves the SDK needs: the dynamic module
// that registers its providers, and the instrumentation hook `NestFactory` has to be
// given at bootstrap. Without the hook the agent reports operations with no spans
// inside them.
export const { ObserveModule, ObserveInstrument } = createObserveModule();
