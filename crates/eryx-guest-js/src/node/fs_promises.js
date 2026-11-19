// node:fs/promises: fs.js's `promises`.
import { promises } from 'node:fs';

export const {
    readFile, writeFile, appendFile, stat, lstat, readdir, mkdir, unlink, rmdir, rm, rename, copyFile, access,
} = promises;
export default promises;
