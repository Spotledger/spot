interface Base { id: string }
interface WithTime extends Base { posted: number }

export const when = (doc: WithTime): number => doc.posted;
