import { CanActivate, ExecutionContext, Injectable } from "@nestjs/common";

@Injectable()
export class AuthGuard implements CanActivate {
  async canActivate(_context: ExecutionContext): Promise<boolean> {
    await new Promise((resolve) => setTimeout(resolve, 3));
    return true;
  }
}
