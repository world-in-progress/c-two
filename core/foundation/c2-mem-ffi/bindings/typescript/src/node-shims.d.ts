declare module "node:module" {
  export interface NodeRequire {
    (specifier: string): unknown;
    resolve(specifier: string): string;
  }

  export function createRequire(filename: string): NodeRequire;
}

declare module "node:net" {
  export interface Socket {
    readonly destroyed: boolean;
    write(buffer: Uint8Array, callback?: () => void): boolean;
    end(): this;
    destroy(error?: Error): this;
    on(event: "data", listener: (chunk: Uint8Array) => void): this;
    on(event: "error", listener: (error: Error) => void): this;
    on(event: "end" | "close", listener: () => void): this;
    once(event: "connect", listener: () => void): this;
    once(event: "error", listener: (error: Error) => void): this;
    once(event: "close", listener: () => void): this;
    off(event: "connect", listener: () => void): this;
    off(event: "error", listener: (error: Error) => void): this;
    off(event: "close", listener: () => void): this;
  }

  export function createConnection(path: string): Socket;
}

interface ImportMeta {
  readonly url: string;
}

declare const process: {
  readonly platform: string;
};
