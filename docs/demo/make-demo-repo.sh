#!/usr/bin/env bash
# Builds the small, invented monorepo the README's demo is recorded on:
# a shop (web → api → database), a billing worker and an admin report job,
# all sharing one connection-pool library. `main` is the base; the branch
# `feature/pool-drain` changes the library, the billing worker and a
# migration — the change prognost is shown on.
#
#   docs/demo/make-demo-repo.sh /tmp/prognost-demo
set -euo pipefail
dir=${1:-/tmp/prognost-demo}
rm -rf "$dir"
mkdir -p "$dir"
cd "$dir"
git init -q -b main
git config user.name "demo"
git config user.email "demo@example.com"

w() { mkdir -p "$(dirname "$1")"; cat > "$1"; }

w package.json <<'EOF'
{ "name": "acme", "private": true }
EOF
w pnpm-workspace.yaml <<'EOF'
packages:
  - "packages/*"
  - "apps/*/*"
EOF

# --- the shared library -------------------------------------------------
w packages/db-pool/package.json <<'EOF'
{ "name": "@acme/db-pool", "main": "src/index.ts" }
EOF
w packages/db-pool/src/index.ts <<'EOF'
import { Pool } from 'pg';

export type PoolOptions = { url: string; max: number };

const resetPool = async (pool: Pool) => {
  await pool.end();
};

export const createPool = (options: PoolOptions): Pool => {
  const pool = new Pool({ connectionString: options.url, max: options.max });
  pool.on('error', () => {
    void resetPool(pool);
  });
  return pool;
};
EOF

# --- shop: database, api, web --------------------------------------------
w apps/shop/database/package.json <<'EOF'
{ "name": "@shop/database", "main": "src/index.ts", "dependencies": { "@acme/db-pool": "workspace:*" } }
EOF
w apps/shop/database/src/index.ts <<'EOF'
export * from './client.ts';
export * from './orders.ts';
EOF
w apps/shop/database/src/client.ts <<'EOF'
import { createPool } from '@acme/db-pool';

export const createClient = () => createPool({ url: process.env.SHOP_DB_URL ?? '', max: 10 });
EOF
w apps/shop/database/src/orders.ts <<'EOF'
import { createClient } from './client.ts';

export const findOrders = async () => {
  const client = createClient();
  return (await client.query('SELECT * FROM orders')).rows;
};

export const insertOrder = async (item: string) => {
  const client = createClient();
  await client.query('INSERT INTO orders (item) VALUES ($1)', [item]);
};
EOF
w apps/shop/database/migrations/0001_init.sql <<'EOF'
CREATE DOMAIN nonempty_text AS TEXT CHECK (VALUE <> '');
CREATE TABLE orders (id serial PRIMARY KEY, item nonempty_text NOT NULL);
EOF

w apps/shop/api/package.json <<'EOF'
{ "name": "@shop/api", "main": "src/index.ts", "dependencies": { "@shop/database": "workspace:*", "hono": "^4" } }
EOF
w apps/shop/api/src/index.ts <<'EOF'
import { Hono } from 'hono';
import { OrderController } from './routes/OrderController.ts';

export const app = new Hono().route('/api/v1/orders', OrderController);
export type AppType = typeof app;
EOF
w apps/shop/api/src/routes/OrderController.ts <<'EOF'
import { Hono } from 'hono';
import * as db from '@shop/database';

export const OrderController = new Hono()
  .get('/', async (c) => {
    return c.json(await db.findOrders());
  })
  .post('/', async (c) => {
    const { item } = await c.req.json();
    await db.insertOrder(item);
    return c.body(null, 201);
  });
EOF

w apps/shop/web/package.json <<'EOF'
{ "name": "@shop/web", "main": "src/cart.ts", "dependencies": { "@shop/api": "workspace:*" } }
EOF
w apps/shop/web/src/cart.ts <<'EOF'
import { hc } from 'hono/client';
import type { AppType } from '@shop/api';

const apiClient = hc<AppType>('/');

export const checkout = async (item: string) => {
  await apiClient.api.v1.orders.$post({ json: { item } });
};

export const loadOrders = async () => {
  return (await apiClient.api.v1.orders.$get()).json();
};
EOF

# --- billing and admin: two more users of the library ---------------------
w apps/billing/worker/package.json <<'EOF'
{ "name": "@billing/worker", "main": "src/main.ts", "dependencies": { "@acme/db-pool": "workspace:*" } }
EOF
w apps/billing/worker/src/main.ts <<'EOF'
import { createPool } from '@acme/db-pool';

const pool = createPool({ url: process.env.BILLING_DB_URL ?? '', max: 4 });

export const runInvoices = async (ids: number[]) => {
  await pool.query('UPDATE invoices SET charged = true WHERE id = ANY($1)', [ids]);
};
EOF
w apps/admin/reports/package.json <<'EOF'
{ "name": "@admin/reports", "main": "src/daily.ts", "dependencies": { "@acme/db-pool": "workspace:*" } }
EOF
w apps/admin/reports/src/daily.ts <<'EOF'
import { createPool } from '@acme/db-pool';

export const dailyReport = async () => {
  const pool = createPool({ url: process.env.ADMIN_DB_URL ?? '', max: 2 });
  return (await pool.query('SELECT count(*) FROM orders')).rows[0];
};
EOF

git add -A
git commit -q -m "Shop, billing and admin on a shared pool"

# --- the change ----------------------------------------------------------
git switch -q -c feature/pool-drain
w packages/db-pool/src/index.ts <<'EOF'
import { Pool } from 'pg';

export type PoolOptions = { url: string; max: number; drainTimeoutMs?: number };

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const drainWaiters = async (pool: Pool, timeoutMs: number) => {
  const until = Date.now() + timeoutMs;
  while (pool.waitingCount > 0 && Date.now() < until) {
    await sleep(25);
  }
};

const resetPool = async (pool: Pool, timeoutMs: number) => {
  await drainWaiters(pool, timeoutMs);
  await pool.end();
};

export const createPool = (options: PoolOptions): Pool => {
  const pool = new Pool({ connectionString: options.url, max: options.max });
  pool.on('error', () => {
    void resetPool(pool, options.drainTimeoutMs ?? 1000);
  });
  return pool;
};
EOF
w apps/billing/worker/src/main.ts <<'EOF'
import { createPool } from '@acme/db-pool';

const pool = createPool({ url: process.env.BILLING_DB_URL ?? '', max: 4, drainTimeoutMs: 500 });

export const runInvoices = async (ids: number[]) => {
  for (const id of ids) {
    await pool.query('UPDATE invoices SET charged = true WHERE id = $1', [id]);
  }
};
EOF
w apps/shop/database/migrations/0002_order_note.sql <<'EOF'
ALTER TABLE orders ADD COLUMN note text NOT NULL;
ALTER TABLE orders ADD COLUMN gift_message nonempty_text;
EOF
git add -A
git commit -q -m "Drain waiting clients before a pool reset"
echo "demo repository ready: $dir (main → feature/pool-drain)"
