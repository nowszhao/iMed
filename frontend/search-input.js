// Keep input scheduling separate from rendering so IME event sequences are testable.
export function bindSearchInput(input, search, invalidate, delay = 450) {
  let timer;
  let composing = false;
  const cancel = () => {
    clearTimeout(timer);
    invalidate();
  };
  const schedule = () => {
    cancel();
    const query = input.value.trim();
    timer = setTimeout(() => search(query), delay);
  };
  input.addEventListener('compositionstart', () => {
    composing = true;
    cancel();
  });
  input.addEventListener('compositionend', () => {
    composing = false;
    schedule();
  });
  input.addEventListener('input', event => {
    if (composing || event.isComposing) cancel();
    else schedule();
  });
  return cancel;
}
