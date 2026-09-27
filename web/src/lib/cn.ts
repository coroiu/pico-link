import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

/** shadcn/ui's standard class-merging helper. */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
