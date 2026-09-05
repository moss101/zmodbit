/** One changed file with numstat counts (additions green, deletions red). */
export function DiffFileRow({
  path,
  additions,
  deletions,
}: {
  path: string;
  additions: number;
  deletions: number;
}) {
  return (
    <li className="modbit-diff-file">
      <span className="modbit-diff-path">{path}</span>
      <span className="modbit-diff-add">+{additions}</span>
      <span className="modbit-diff-del">−{deletions}</span>
    </li>
  );
}
