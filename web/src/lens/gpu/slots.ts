/**
 * Slot bookkeeping for a texture array: which poster lives in which layer, and
 * which one to evict (the least recently drawn, never one drawn this frame).
 * Pure, so it is easy to test; posters.ts owns the textures.
 */
export class Slots {
  capacity: number;
  private keys: (string | null)[] = [];
  private used: number[] = [];
  private index = new Map<string, number>();

  constructor(capacity: number) {
    this.capacity = capacity;
  }

  get size(): number {
    return this.index.size;
  }

  find(key: string): number {
    return this.index.get(key) ?? -1;
  }

  /** Mark a slot as drawn in `frame`. */
  touch(slot: number, frame: number): void {
    this.used[slot] = frame;
  }

  grow(capacity: number): void {
    this.capacity = Math.max(this.capacity, capacity);
  }

  /**
   * A slot for `key`: a free one, else the least recently used one not drawn
   * since `protectSince` (its key is returned as `evicted`). -1 when every slot
   * is in use. Several lenses can share the textures, each advancing the frame,
   * so a caller protects the last few frames rather than only the current one.
   */
  claim(key: string, frame: number, protectSince = frame): { slot: number; evicted: string | null } {
    const have = this.index.get(key);
    if (have !== undefined) {
      this.used[have] = frame;
      return { slot: have, evicted: null };
    }
    let slot = -1;
    if (this.keys.length < this.capacity) slot = this.keys.length;
    else {
      let oldest = Infinity;
      for (let s = 0; s < this.keys.length; s++) {
        if (this.keys[s] === null) {
          slot = s;
          break;
        }
        if (this.used[s] < protectSince && this.used[s] < oldest) {
          oldest = this.used[s];
          slot = s;
        }
      }
    }
    if (slot < 0) return { slot: -1, evicted: null };
    const evicted = this.keys[slot] ?? null;
    if (evicted !== null) this.index.delete(evicted);
    this.keys[slot] = key;
    this.used[slot] = frame;
    this.index.set(key, slot);
    return { slot, evicted };
  }

  release(key: string): void {
    const s = this.index.get(key);
    if (s === undefined) return;
    this.index.delete(key);
    this.keys[s] = null;
  }

  clear(): void {
    this.keys = [];
    this.used = [];
    this.index.clear();
  }
}
