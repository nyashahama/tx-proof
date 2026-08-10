import { createOpenGraphCard, openGraphContentType, openGraphSize } from "@/components/metadata/open-graph-card";
import { openGraphCards } from "@/content/open-graph";

export const alt = openGraphCards["/safety"].title;
export const size = openGraphSize;
export const contentType = openGraphContentType;

export default function Image() { return createOpenGraphCard(openGraphCards["/safety"]); }
