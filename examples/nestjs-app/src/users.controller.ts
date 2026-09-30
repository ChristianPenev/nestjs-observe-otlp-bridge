import {
  Controller,
  Get,
  Param,
  UseGuards,
  UseInterceptors,
} from "@nestjs/common";
import { AuthGuard } from "./auth.guard.js";
import { LoggingInterceptor } from "./logging.interceptor.js";
import { UsersService } from "./users.service.js";

@Controller("users")
@UseGuards(AuthGuard)
@UseInterceptors(LoggingInterceptor)
export class UsersController {
  constructor(private readonly usersService: UsersService) {}

  // The route that produces the trace shape in the README: request > controller >
  // (guard, service > nested call).
  @Get(":id")
  getUser(@Param("id") id: string) {
    return this.usersService.findUser(id);
  }

  // Throws a NotFoundException, so the exported span carries an exception event and
  // an error status.
  @Get("missing/:id")
  getMissing(@Param("id") id: string) {
    return this.usersService.findUser("does-not-exist");
  }

  // Fails with a plain error, which becomes a 500 and a 5xx span status.
  @Get("boom/all")
  boom() {
    throw new Error("deliberate failure from the example app");
  }
}
