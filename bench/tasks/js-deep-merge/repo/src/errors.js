/**
 * Raised for configuration problems the caller should report to a human:
 * an unreadable or malformed config file, or required settings that are missing.
 */
export class ConfigError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = 'ConfigError';
  }
}
