"""Model-free labeling regression for the actual bakeoff summary logic."""
import math
import unittest
from semantic_metrics import summarize

class SemanticMetricLabels(unittest.TestCase):
    def test_multi_relevant_judgments_are_labeled_as_first_hit_measures(self):
        # Query A has two relevant symbols, at ranks 1 and 30. The second
        # symbol is absent from top10 but intentionally cannot alter first-hit measures.
        relevant_ranks = [[1,30], [5,8], []]
        metrics = summarize(min(ranks) if ranks else None for ranks in relevant_ranks)
        self.assertEqual(set(metrics), {"hit_rate@1","hit_rate@5","hit_rate@10","mrr@10","first_hit_discount@10"})
        self.assertEqual(metrics["hit_rate@1"], 1/3)
        self.assertEqual(metrics["hit_rate@10"], 2/3)
        self.assertAlmostEqual(metrics["mrr@10"], (1+1/5)/3)
        self.assertAlmostEqual(metrics["first_hit_discount@10"], (1+1/math.log2(6))/3)
        self.assertEqual(summarize([1]), summarize([min([1,30])]))

if __name__ == "__main__": unittest.main()
