// Counts each item's quantity in stock.
export class Ledger {
  constructor() {
    this.items = new Map();
  }

  add(item, quantity) {
    this.items.set(item, (this.items.get(item) ?? 0) + quantity);
  }

  take(item, quantity) {
    const left = (this.items.get(item) ?? 0) - quantity;
    if (left < 0) {
      throw new Error(`only ${this.items.get(item) ?? 0} of ${item} in stock`);
    }
    this.items.set(item, left);
  }

  count(item) {
    return this.items.get(item) ?? 0;
  }
}
