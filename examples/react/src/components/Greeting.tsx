export default function Greeting({ name, children }: { name: string; children?: React.ReactNode }) {
  return (
    <section className="greeting">
      <h2>Hello, {name}</h2>
      {children}
    </section>
  );
}
