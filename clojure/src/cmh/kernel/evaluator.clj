(ns cmh.kernel.evaluator
  "Evaluates userspace Clojure. Validation loads source into a fresh,
  throwaway scratch namespace; live versions each get their own namespace
  cmh.user.<name>.v<version>. The kernel resolves the entry var by name at
  every call, so a new live version is used on the very next call."
  (:require [clojure.data.json :as json]
            [clojure.string :as str])
  (:import [java.util.concurrent ExecutionException TimeoutException]))

(defn with-timeout
  "Run f on another thread; throw TimeoutException after ms."
  [ms f]
  (let [fut (future (f))
        r (try (deref fut ms ::timeout)
               (catch ExecutionException e (throw (or (.getCause e) e))))]
    (when (= r ::timeout)
      (future-cancel fut)
      (throw (TimeoutException. (str "timed out after " ms " ms"))))
    r))

(defn describe-error [^Throwable e]
  (let [root (loop [t e] (if-let [c (.getCause t)] (recur c) t))
        msg (fn [^Throwable t] (str (.getSimpleName (class t))
                                    (when-let [m (.getMessage t)] (str ": " m))))]
    (if (identical? root e) (msg e) (str (msg e) " (cause " (msg root) ")"))))

(def hooks
  "The hooks the kernel calls, by name: entry-point symbol, how a map input
  (live call or test input) becomes the argument vector, and the return shape
  (:string, or :ids for a sequence of lesson ids)."
  {"compose_system" {:entry 'compose-system
                     :args (fn [input] [(vec (:notes input)) (vec (:lessons input))])
                     :returns :string}
   "select_lessons" {:entry 'select-lessons
                     :args (fn [input] [(str (:task input)) (vec (:lessons input))])
                     :returns :ids}})

(defn- hook [kind name] (when (= kind "hook") (get hooks name)))

(defn entry-sym
  "The fixed entry-point name for a definition."
  [kind name]
  (case kind
    "tool" 'run
    "hook" (:entry (hook kind name))
    nil))

(defn load-into!
  "Create namespace ns-sym, which must not exist yet, and evaluate source
  inside it. On failure the namespace is removed again."
  [ns-sym source ms]
  (let [n (create-ns ns-sym)]
    (try
      (with-timeout ms (fn []
                         (binding [*ns* n]
                           (refer-clojure)
                           (load-string source))))
      n
      (catch Throwable e (remove-ns ns-sym) (throw e)))))

(defn entry-fn [n sym]
  (let [v (get (ns-interns n) sym)]
    (when (and (var? v) (fn? @v)) v)))

(defn- check-return [kind name r]
  (if (= :ids (:returns (hook kind name)))
    (if (and (or (nil? r) (sequential? r))
             (every? #(or (string? %) (number? %)) r))
      (vec r)
      (throw (ex-info (str "bad return type: select-lessons must return a sequence of ids, got "
                           (pr-str r)) {})))
    (if (string? r)
      r
      (throw (ex-info (str "bad return type: expected string, got "
                           (if (nil? r) "nil" (.getName (class r)))) {})))))

(defn args-for
  "Argument vector for a kind/name from a map input (tool input or hook test input)."
  [kind name input]
  (if-let [h (hook kind name)]
    ((:args h) input)
    [input]))

(defn invoke
  "Call entry var v with args under a timeout and check the return shape."
  [kind name v args ms]
  (check-return kind name (with-timeout ms #(apply @v args))))

(defn run-tests
  "Returns nil when all tests pass, else an error string."
  [kind name v tests ms]
  (some (fn [[i {:keys [input expect]}]]
          (try
            (let [r (invoke kind name v (args-for kind name input) ms)
                  out (if (string? r) r (json/write-str r))]
              (when-not (str/includes? out (str expect))
                (str "test " (inc i) " failed: input " (json/write-str input)
                     " expected substring " (pr-str (str expect))
                     " in output " (pr-str (subs out 0 (min 500 (count out)))))))
            (catch Throwable e
              (str "test " (inc i) " threw " (describe-error e)
                   " on input " (json/write-str input)))))
        (map-indexed vector tests)))

(defn validate-in-scratch
  "Evaluate source in a fresh scratch namespace, check the entry point and
  run the tests. The scratch namespace is always removed, so a failing
  proposal never touches the live version's namespace. nil = ok."
  [{:keys [kind name source tests]} ms]
  (let [ns-sym (symbol (str "cmh.scratch." name "." (random-uuid)))]
    (try
      (let [n (load-into! ns-sym source ms)
            sym (entry-sym kind name)]
        (if-let [v (entry-fn n sym)]
          (run-tests kind name v tests ms)
          (str "source must define (defn " sym " ...)")))
      (catch Throwable e (str "eval failed: " (describe-error e)))
      (finally (when (find-ns ns-sym) (remove-ns ns-sym))))))

(defn- source-hash [^String s]
  (let [d (.digest (java.security.MessageDigest/getInstance "SHA-256") (.getBytes s "UTF-8"))]
    (apply str (map #(format "%02x" (bit-and % 0xff)) (take 6 d)))))

(defn live-ns-sym
  "One namespace per definition version. The source hash keeps two kernels
  (different state dirs) in one JVM from sharing a namespace."
  [name version source]
  (symbol (str "cmh.user." name ".v" version "-" (source-hash source))))

(def ^:private load-lock (Object.))

(defn live-entry
  "Resolve the entry var of a live record, loading its namespace on first use."
  [{:keys [kind name version source]} ms]
  (let [ns-sym (live-ns-sym name version source)
        n (locking load-lock (or (find-ns ns-sym) (load-into! ns-sym source ms)))]
    (or (entry-fn n (entry-sym kind name))
        (throw (ex-info (str "entry point " (entry-sym kind name) " missing in " ns-sym) {})))))
