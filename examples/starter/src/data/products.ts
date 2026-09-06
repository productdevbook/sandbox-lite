export interface Product {
  slug: string;
  name: string;
  blurb: string;
  price: number;
}

export const products: Product[] = [
  { slug: "brand-sprint", name: "Brand sprint", blurb: "A focused week to name, position and visually define a product.", price: 4200 },
  { slug: "site-refresh", name: "Site refresh", blurb: "Your existing site, rebuilt on a fast static stack with a content workflow.", price: 6800 },
  { slug: "retainer", name: "Design retainer", blurb: "A monthly block of design hours for teams that ship continuously.", price: 2400 },
];

export function formatPrice(value: number, currency = "EUR"): string {
  return new Intl.NumberFormat("en", { style: "currency", currency, maximumFractionDigits: 0 }).format(value);
}
