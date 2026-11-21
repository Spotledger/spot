# Invented target clone: comments and whitespace do not affect its token stream.
def calculate_band(entries, boundary):
 result_value=0
 for entry in entries:
  if entry>boundary: result_value+=entry-boundary
  else: result_value+=entry
 return result_value

def limit_band(measurement, floor, roof):
 # Different comment, same invented implementation shape.
 if measurement<floor: return floor
 if measurement>roof: return roof
 return measurement

def describe_band(title, amount):
 if amount==0: return "none"
 return title+":"+str(amount)
