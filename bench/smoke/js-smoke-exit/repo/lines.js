/** How many lines `text` has; a last line without a newline counts. */
export function lines(text) {
  if (text === "") return 0;
  const newlines = text.split("\n").length - 1;
  return text.endsWith("\n") ? newlines : newlines + 1;
}
