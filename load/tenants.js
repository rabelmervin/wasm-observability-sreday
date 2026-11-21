// Steady background traffic for all three tenants, so the dashboard is "alive"
// before anything breaks.
//
//   k6 run --address "" load/tenants.js      (Ctrl+C to stop)
import http from 'k6/http';
import { check } from 'k6';

const BASE = __ENV.BASE_URL || 'http://localhost:8000';   // the wash host on this machine

export const options = {
  scenarios: {
    'app-a': { executor: 'constant-arrival-rate', rate: 5, timeUnit: '1s', duration: '2h', preAllocatedVUs: 20, exec: 'appA' },
    'app-b': { executor: 'constant-arrival-rate', rate: 8, timeUnit: '1s', duration: '2h', preAllocatedVUs: 20, exec: 'appB' },
    'app-c': { executor: 'constant-arrival-rate', rate: 3, timeUnit: '1s', duration: '2h', preAllocatedVUs: 20, exec: 'appC' },
  },
  // On stage we *want* failures to be visible, so never abort on thresholds.
  thresholds: {},
  // The host container can get a new IP when restarted; don't cache DNS for 5 min.
  dns: { ttl: '1s' },
};

function call(tenant, fn, body) {
  const res = http.post(`${BASE}/t/${tenant}/${fn}`, JSON.stringify(body), {
    headers: { 'content-type': 'application/json' },
    tags: { tenant, fn },
    timeout: '10s',
  });
  check(res, { 'status is 2xx': (r) => r.status >= 200 && r.status < 300 });
}

const SKUS = ['sku-1', 'sku-2', 'sku-3', 'sku-7'];

export function appA() { call('app-a', 'invoice-pdf', { customer: 'c-' + Math.floor(Math.random() * 100), amount: 120 }); }
export function appB() { call('app-b', 'price-calc', { sku: SKUS[Math.floor(Math.random() * SKUS.length)], qty: 1 + Math.floor(Math.random() * 5) }); }
export function appC() { call('app-c', 'data-sync', { batch: Math.floor(Math.random() * 1000) }); }
