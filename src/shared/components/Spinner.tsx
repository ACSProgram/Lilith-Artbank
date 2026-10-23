export function Spinner({ size = 16 }: { size?: number }) {
  return <span className="spinner" style={{ width: size, height: size }} aria-hidden="true" />;
}
