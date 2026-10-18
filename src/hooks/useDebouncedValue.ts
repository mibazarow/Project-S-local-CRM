import { useEffect, useState } from "react";

// Returns `value` delayed by `delay` ms: each change restarts the timer, and the
// pending timer is cleared on unmount. Mirrors the inline debounce the views
// used to hand-roll (setActiveQuery after a setTimeout).
export function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(timer);
  }, [value, delay]);
  return debounced;
}
