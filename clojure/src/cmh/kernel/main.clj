(ns cmh.kernel.main
  "CLI entry point: clojure -M:run [opts] <command>."
  (:require [clojure.data.json :as json]
            [clojure.string :as str]
            [cmh.kernel.agent :as agent]
            [cmh.kernel.gate :as gate]
            [cmh.kernel.provider :as provider]
            [cmh.kernel.state :as st]
            [cmh.kernel.tools :as tools]))

(def usage
  "cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] <command>

  repl            interactive; each line is a task. Slash commands:
                  /defs  /show NAME [VERSION]  /knowledge  /refine
                  /rollback NAME VERSION  /quit
  run \"TASK\"      one task, prints the final answer, exits 0 on success
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts")

(defn parse-args [args]
  (loop [opts {:state ".cmh" :provider "anthropic" :soak 3 :reflect true
               :model (or (System/getenv "CMH_MODEL") "claude-sonnet-5-5")}
         [a & more :as args] args]
    (case a
      "--state" (recur (assoc opts :state (first more)) (rest more))
      "--provider" (recur (assoc opts :provider (first more)) (rest more))
      "--script" (recur (assoc opts :script (first more)) (rest more))
      "--model" (recur (assoc opts :model (first more)) (rest more))
      "--soak" (recur (assoc opts :soak (parse-long (str (first more)))) (rest more))
      "--no-reflect" (recur (assoc opts :reflect false) more)
      (assoc opts :command a :args (vec more)))))

(defn knowledge-text [s]
  (let [ls (st/lessons s) vs (st/verdicts s)]
    (str "Lessons:\n"
         (if (seq ls)
           (str/join "\n" (map #(str "  " (:id %) " [score " (:score %) (when (:retired %) ", retired") "] " (:text %)) ls))
           "  (none)")
         "\nVerdicts:\n"
         (if (seq vs) (str/join "\n" (map #(str "  " (:id %) " " (:text %)) vs)) "  (none)"))))

(defn- show-text [k name version]
  (:content (tools/run-tool k "show_def" (cond-> {:name name} version (assoc :version version)))))

(defn- repl! [k]
  (let [say #(do (println %) (flush))]
    (loop []
      (binding [*out* *err*] (print "cmh> ") (flush))
      (when-let [line (read-line)]
        (let [line (str/trim line)
              [cmd & xs] (str/split line #"\s+")]
          (cond
            (str/blank? line) (recur)
            (= cmd "/quit") nil
            :else
            (do (case cmd
                  "/defs" (say (tools/defs-table @(:state k)))
                  "/knowledge" (say (knowledge-text @(:state k)))
                  "/show" (say (show-text k (first xs) (second xs)))
                  "/rollback" (say (:msg (gate/rollback! k (first xs) (parse-long (str (second xs))))))
                  "/refine" (agent/refine! k {:on-text say})
                  (if (str/starts-with? cmd "/")
                    (say (str "unknown command " cmd))
                    (agent/run-task! k line {:on-text say})))
                (recur))))))
    0))

(defn run-cli
  "Run the CLI. Returns {:exit code :kernel k}."
  [args]
  (let [{:keys [command] :as opts} (parse-args args)
        needs-model (#{"repl" "run" "refine"} command)]
    (if-not (#{"repl" "run" "refine" "defs" "knowledge"} command)
      (do (binding [*out* *err*] (println usage)) {:exit 2})
      (let [prov (when needs-model
                   (case (:provider opts)
                     "mock" (provider/mock-from-file
                             (or (:script opts) (throw (ex-info "--provider mock needs --script FILE" {}))))
                     "anthropic" (provider/anthropic-provider {:model (:model opts)})
                     (throw (ex-info (str "unknown provider " (:provider opts)) {}))))
            k (agent/open {:state-dir (:state opts) :provider prov
                           :soak (:soak opts) :reflect (:reflect opts)})
            final (fn [r] (when-not (str/blank? (:answer r)) (println (:answer r))) (flush)
                    (if (:success r) 0 1))
            exit (case command
                   "defs" (do (println (tools/defs-table @(:state k))) 0)
                   "knowledge" (do (println (knowledge-text @(:state k))) 0)
                   "run" (if-let [task (first (:args opts))]
                           (final (agent/run-task! k task))
                           (do (binding [*out* *err*] (println "run needs a TASK")) 2))
                   "refine" (final (agent/refine! k {}))
                   "repl" (repl! k))]
        {:exit exit :kernel k}))))

(defn -main [& args]
  (let [code (try (:exit (run-cli args))
                  (catch Throwable e
                    (binding [*out* *err*] (println "cmh:" (or (.getMessage e) (str e))))
                    1))]
    (flush)
    (shutdown-agents)
    (System/exit code)))
