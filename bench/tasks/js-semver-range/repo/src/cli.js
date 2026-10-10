// `pkgver` command line. run() takes the arguments and an output sink and
// returns the exit code, so it can be tested without spawning a process.

import { compare, inc, sort, rsort, valid } from './semver.js';
import { satisfies } from './range.js';
import { maxSatisfying, minSatisfying } from './select.js';

export const USAGE = `usage: pkgver <command> [args]

commands:
  valid <version>                    print the normalized version (exit 1 if invalid)
  compare <a> <b>                    print -1, 0 or 1
  sort [-r] <version>...             print the versions in ascending (or descending) order
  inc <version> <release> [id]       print the next version (major, minor, patch, prerelease)
  satisfies <version> <range>        print true or false (exit 1 if false)
  max <range> <version>...           print the highest version in the range (exit 1 if none)
  min <range> <version>...           print the lowest version in the range (exit 1 if none)
`;

const defaultIo = {
  out: (s) => process.stdout.write(s),
  err: (s) => process.stderr.write(s),
};

function need(args, n, command) {
  if (args.length < n) throw new UsageError(`${command}: expected at least ${n} argument(s)`);
}

class UsageError extends Error {}

const commands = {
  valid(args, io) {
    need(args, 1, 'valid');
    const v = valid(args[0]);
    if (v === null) {
      io.err(`pkgver: invalid version: ${args[0]}\n`);
      return 1;
    }
    io.out(`${v}\n`);
    return 0;
  },

  compare(args, io) {
    need(args, 2, 'compare');
    io.out(`${compare(args[0], args[1])}\n`);
    return 0;
  },

  sort(args, io) {
    const reverse = args[0] === '-r';
    const versions = reverse ? args.slice(1) : args;
    for (const v of reverse ? rsort(versions) : sort(versions)) io.out(`${v}\n`);
    return 0;
  },

  inc(args, io) {
    need(args, 2, 'inc');
    const next = inc(args[0], args[1], args[2]);
    if (next === null) {
      io.err(`pkgver: invalid version: ${args[0]}\n`);
      return 1;
    }
    io.out(`${next}\n`);
    return 0;
  },

  satisfies(args, io) {
    need(args, 2, 'satisfies');
    const ok = satisfies(args[0], args[1]);
    io.out(`${ok}\n`);
    return ok ? 0 : 1;
  },

  max(args, io) {
    need(args, 1, 'max');
    return printPick(maxSatisfying(args.slice(1), args[0]), io);
  },

  min(args, io) {
    need(args, 1, 'min');
    return printPick(minSatisfying(args.slice(1), args[0]), io);
  },
};

function printPick(version, io) {
  if (version === null) return 1;
  io.out(`${version}\n`);
  return 0;
}

/**
 * @param {string[]} argv  arguments after the program name
 * @param {{out: (s: string) => void, err: (s: string) => void}} [io]
 * @returns {number} exit code: 0 ok, 1 negative answer, 2 usage or input error
 */
export function run(argv, io = defaultIo) {
  const [name, ...args] = argv;
  if (name === undefined || name === 'help' || name === '-h' || name === '--help') {
    io.out(USAGE);
    return name === undefined ? 2 : 0;
  }
  const command = Object.hasOwn(commands, name) ? commands[name] : null;
  if (command === null) {
    io.err(`pkgver: unknown command: ${name}\n${USAGE}`);
    return 2;
  }
  try {
    return command(args, io);
  } catch (err) {
    io.err(`pkgver: ${err.message}\n`);
    return 2;
  }
}
