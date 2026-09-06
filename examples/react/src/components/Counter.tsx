import { useState } from "react";

interface Props {
  label: string;
  start?: number;
}

export default function Counter({ label, start = 0 }: Props) {
  const [n, setN] = useState(start);
  return (
    <div className="counter" data-count={n}>
      <span>{label}</span>
      <button type="button" onClick={() => setN(n - 1)} aria-label="decrement">−</button>
      <strong>{n}</strong>
      <button type="button" onClick={() => setN(n + 1)} aria-label="increment">+</button>
    </div>
  );
}
