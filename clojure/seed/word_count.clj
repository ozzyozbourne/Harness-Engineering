;; Tool: (run input) -> string, input is a keyword-keyed map.
(defn run [input]
  (str (count (re-seq #"\S+" (str (:text input))))))
