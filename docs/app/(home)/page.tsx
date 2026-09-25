import Link from 'next/link';

const features = [
  {
    title: 'One file to edit',
    body: 'Tools, system prompt and model live in src/agent.rs. Everything else stays as is.',
  },
  {
    title: 'Persistent sessions',
    body: 'Every conversation, tool calls included, is stored in SQLite and resumes where it left off.',
  },
  {
    title: 'Three transports',
    body: 'A CLI, an HTTP API with SSE streaming, and a Telegram bot, all over one core service.',
  },
  {
    title: 'Cost you can see',
    body: 'Each turn records model calls and tokens by kind, so usage is a query away.',
  },
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col items-center px-4 py-16 md:py-24">
      <section className="max-w-2xl text-center">
        <p className="mb-3 text-sm font-medium text-fd-muted-foreground">
          Rust · rig-agent · SQLite · OpenRouter
        </p>
        <h1 className="mb-4 text-4xl font-bold tracking-tight md:text-5xl">
          Persistent, tool-using agents in Rust
        </h1>
        <p className="mb-8 text-lg text-fd-muted-foreground">
          athena is a template: define your agent in one file and get storage,
          sessions, usage tracking and three transports for free.
        </p>
        <div className="flex flex-wrap justify-center gap-3">
          <Link
            href="/docs/getting-started"
            className="rounded-full bg-fd-primary px-5 py-2.5 text-sm font-medium text-fd-primary-foreground"
          >
            Get started
          </Link>
          <Link
            href="/docs"
            className="rounded-full border px-5 py-2.5 text-sm font-medium"
          >
            Read the docs
          </Link>
        </div>
      </section>

      <section className="mt-16 grid w-full max-w-4xl gap-4 sm:grid-cols-2">
        {features.map((feature) => (
          <div key={feature.title} className="rounded-xl border bg-fd-card p-5">
            <h2 className="mb-1 font-semibold">{feature.title}</h2>
            <p className="text-sm text-fd-muted-foreground">{feature.body}</p>
          </div>
        ))}
      </section>
    </main>
  );
}
