import { Injectable, Logger, NotFoundException } from "@nestjs/common";

@Injectable()
export class UsersService {
  private readonly logger = new Logger(UsersService.name);

  private readonly users = new Map<string, { id: string; name: string }>([
    ["1", { id: "1", name: "Ada" }],
    ["2", { id: "2", name: "Grace" }],
  ]);

  async findUser(id: string) {
    // Stands in for a database call, so the span has a duration worth looking at.
    await this.readFromStore(id);

    const user = this.users.get(id);
    if (!user) {
      this.logger.warn(`No user with id ${id}`);
      throw new NotFoundException(`No user with id ${id}`);
    }

    this.logger.log(`Loaded user ${id}`);
    return user;
  }

  // A separate method so the trace shows a nested call rather than one flat span.
  private async readFromStore(id: string) {
    await new Promise((resolve) => setTimeout(resolve, 12));
    return id;
  }
}
