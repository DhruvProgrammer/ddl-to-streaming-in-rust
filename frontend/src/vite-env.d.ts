/// <reference types="vite/client" />

// Vite injects a stylesheet as a side-effect-only import; TypeScript needs to be
// told that this is legal without pulling in a CSS module type package.
declare module "*.css";