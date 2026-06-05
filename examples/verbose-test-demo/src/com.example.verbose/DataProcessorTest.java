package com.example.verbose;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Produces exactly 20 000 lines of stdout output (20 batches × 1 000 lines).
 *
 * The primary purpose is to exercise curie inspect's virtualized log renderer
 * with a realistic large log.  Run the build then inspect:
 *
 *   curie build
 *   curie inspect
 */
class DataProcessorTest {

    private static final int LINES_PER_BATCH = 1_000;

    @Test void batch_01() { runBatch(1); }
    @Test void batch_02() { runBatch(2); }
    @Test void batch_03() { runBatch(3); }
    @Test void batch_04() { runBatch(4); }
    @Test void batch_05() { runBatch(5); }
    @Test void batch_06() { runBatch(6); }
    @Test void batch_07() { runBatch(7); }
    @Test void batch_08() { runBatch(8); }
    @Test void batch_09() { runBatch(9); }
    @Test void batch_10() { runBatch(10); }
    @Test void batch_11() { runBatch(11); }
    @Test void batch_12() { runBatch(12); }
    @Test void batch_13() { runBatch(13); }
    @Test void batch_14() { runBatch(14); }
    @Test void batch_15() { runBatch(15); }
    @Test void batch_16() { runBatch(16); }
    @Test void batch_17() { runBatch(17); }
    @Test void batch_18() { runBatch(18); }
    @Test void batch_19() { runBatch(19); }
    @Test void batch_20() { runBatch(20); }

    private static void runBatch(int batchId) {
        var items = DataProcessor.generate("item-" + batchId, LINES_PER_BATCH);
        assertEquals(LINES_PER_BATCH, items.size(),
                "generate() must return exactly LINES_PER_BATCH items");

        for (int i = 0; i < items.size(); i++) {
            System.out.printf("[batch-%02d] %4d/%d  %s%n",
                    batchId, i + 1, LINES_PER_BATCH, items.get(i));
        }

        assertEquals("item-" + batchId + "-1", items.get(0));
        assertEquals("item-" + batchId + "-" + LINES_PER_BATCH,
                items.get(LINES_PER_BATCH - 1));
    }
}
