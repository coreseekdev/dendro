TLATOOLS = tools/tla2tools.jar

check:
	java -cp $(TLATOOLS) tlc2.TLC -cleanup -config commit-pipeline.cfg CommitPipeline.tla 2>&1 | tail -5

liveness:
	java -cp $(TLATOOLS) tlc2.TLC -cleanup -config commit-pipeline-liveness.cfg CommitPipeline.tla 2>&1 | tail -5

.PHONY: check liveness
