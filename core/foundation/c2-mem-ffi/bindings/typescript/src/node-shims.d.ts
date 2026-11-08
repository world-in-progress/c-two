declare module "node:module" {
  export interface NodeRequire {
    (specifier: string): unknown;
    resolve(specifier: string): string;
  }

  export function createRequire(filename: string): NodeRequire;
}

interface ImportMeta {
  readonly url: string;
}

declare const process: {
  readonly platform: string;
};
