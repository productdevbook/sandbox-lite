import { formatPrice, products } from "@/data/products";

export function GET(): Response {
  const body = products.map((p) => ({ ...p, price: formatPrice(p.price) }));
  return new Response(JSON.stringify(body, null, 2), { headers: { "content-type": "application/json; charset=utf-8" } });
}
