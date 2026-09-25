import { Inter } from 'next/font/google';
import { Provider } from '@/components/provider';
import type { Metadata } from 'next';
import './global.css';

// Absolute base for Open Graph image URLs. CI sets it to the Pages URL.
export const metadata: Metadata = {
  metadataBase: new URL(process.env.DOCS_SITE_URL ?? 'http://localhost:3000'),
};

const inter = Inter({
  subsets: ['latin'],
});

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="en" className={inter.className} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen">
        <Provider>{children}</Provider>
      </body>
    </html>
  );
}
