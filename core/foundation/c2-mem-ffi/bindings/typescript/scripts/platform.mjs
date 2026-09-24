export function nativeLibraryName(platform = process.platform) {
  const names = {
    darwin: 'libc2_mem_ffi.dylib',
    linux: 'libc2_mem_ffi.so',
    win32: 'c2_mem_ffi.dll',
  };
  const name = names[platform];
  if (name === undefined) {
    throw new Error(`c2-mem-ffi native library is not supported on ${platform}.`);
  }
  return name;
}
