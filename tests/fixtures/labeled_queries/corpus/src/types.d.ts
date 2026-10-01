// Ambient declarations for the gatehouse bindings.
// `src/types.d.ts` mirrors the Rust surface for TypeScript consumers.

declare module "gatehouse" {
  export interface Session {
    subject: string;
    expiresAt: number;
  }
}
