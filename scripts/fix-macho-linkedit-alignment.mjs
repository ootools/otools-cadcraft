/**
 * Work around a macOS linker/dyld mismatch in large Rust cdylibs.
 *
 * Clang's Mach-O writer can place LC_SYMTAB.stroff at a 4-byte boundary while
 * dyld 6 requires the symbol string pool to be 8-byte aligned. The resulting
 * dylib is rejected with "mis-aligned LINKEDIT string pool". We insert four
 * zero bytes before both the string pool and the code signature, then update
 * the section offsets. String indexes are relative to the pool, so their
 * values stay valid; dyldinfo/export data all precede the pool.
 *
 * This script is macOS-only and a no-op when the pool is already aligned.
 * The release pipeline re-signs after this script runs.
 *
 * Usage: node scripts/fix-macho-linkedit-alignment.mjs <dylib>
 */
import fs from 'node:fs';

const path = process.argv[2];
if (!path) {
  console.error('usage: node scripts/fix-macho-linkedit-alignment.mjs <dylib>');
  process.exit(2);
}

const bytes = fs.readFileSync(path);
if (bytes.readUInt32LE(0) !== 0xfeedfacf) {
  console.error(`not a thin 64-bit Mach-O: ${path}`);
  process.exit(1);
}
const ncmds = bytes.readUInt32LE(16);
let offset = 32;
let symtab = -1;
let dysymtab = -1;
let linkedit = -1;
let signature = -1;
for (let i = 0; i < ncmds; i += 1) {
  const command = bytes.readUInt32LE(offset);
  const size = bytes.readUInt32LE(offset + 4);
  if (command === 0x2) symtab = offset;
  else if (command === 0xb) dysymtab = offset;
  else if (command === 0x1d) signature = offset;
  else if (command === 0x19) {
    const name = bytes.subarray(offset + 8, offset + 24).toString('utf8').replace(/\0+$/, '');
    if (name === '__LINKEDIT') linkedit = offset;
  }
  offset += size;
}
if ([symtab, dysymtab, linkedit, signature].includes(-1)) {
  console.error('missing required Mach-O load commands');
  process.exit(1);
}

const symoff = bytes.readUInt32LE(symtab + 8);
const nsyms = bytes.readUInt32LE(symtab + 12);
const stroff = bytes.readUInt32LE(symtab + 16);
const strsize = bytes.readUInt32LE(symtab + 20);
const pad = (8 - (stroff % 8)) % 8;
if (pad === 0) {
  console.log('LINKEDIT symbol pool already aligned');
  process.exit(0);
}
if (pad !== 4) {
  console.error(`unsupported misalignment: ${pad} bytes`);
  process.exit(1);
}
const indirectOff = bytes.readUInt32LE(dysymtab + 48);
if (indirectOff >= stroff) {
  console.error('indirect symbol table unexpectedly follows the string pool');
  process.exit(1);
}
const sigOff = bytes.readUInt32LE(signature + 8);
if (sigOff < stroff + strsize) {
  console.error('code signature overlaps the symbol string pool');
  process.exit(1);
}

// Insert padding before the string pool; its contents and all indexes shift together.
const withPoolPadding = Buffer.concat([
  bytes.subarray(0, stroff),
  Buffer.alloc(pad),
  bytes.subarray(stroff),
]);
// The code signature follows the pool. It must also start at an 8-byte
// boundary, so insert pad bytes immediately before it as well.
const finalSigOff = sigOff + pad;
const padded = Buffer.concat([
  withPoolPadding.subarray(0, finalSigOff),
  Buffer.alloc(pad),
  withPoolPadding.subarray(finalSigOff),
]);
padded.writeUInt32LE(stroff + pad, symtab + 16);
padded.writeUInt32LE(finalSigOff + pad, signature + 8);
// __LINKEDIT grows by both insertions.
const linkeditFileSize = padded.readBigUInt64LE(linkedit + 48);
padded.writeBigUInt64LE(linkeditFileSize + BigInt(pad * 2), linkedit + 48);

fs.writeFileSync(path, padded);
console.log(`aligned Mach-O string pool: ${stroff} -> ${stroff + pad}; signature: ${sigOff} -> ${finalSigOff + pad}`);
